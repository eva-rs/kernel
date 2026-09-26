//! `Module` — билдер модуля и `Connected` — живое подключение с
//! реконнектом (`06 §8`, `§10`).

use std::{
    collections::BTreeMap,
    pin::pin,
    sync::{Arc, PoisonError, RwLock},
    time::Duration,
};

use eva::{_priv::schemars::JsonSchema, time::HumanDuration, url::Url};
use serde::de::DeserializeOwned;
use tokio::time::Instant;

use eva_core::{Caller, Provider};
use eva_protocol::{
    types::{
        ModuleId, SessionId,
        tool::{GroupDecl, Need, ResultForm, ToolDecl},
    },
    wire::{
        Capabilities, ErrorCode, Hello, RpcError, Welcome,
        capabilities::{EventsCap, FailMode, HooksCap, ProviderCap, ToolsCap},
        events::{Event, Topic},
        handshake::{ProtoVersion, Resume},
        hooks::{Decision, HookPoint},
        tools::ToolResult,
    },
};
use eva_rpc::{ClientOptions, Conn, ConnectError, IdSeq, RpcClient};

use crate::{
    Kernel,
    backoff::{self, Backoff},
    config::{DEFAULT_CONNECT_WAIT, ModuleConfig},
    handler::{
        ConnState, Current, DynProvider, EventFn, HookInput, HookSlots, Registry, SdkHandler, Slot,
    },
    surface::Surface,
    tool::{self, ToolCtx, ToolEntry, ToolError},
};

/// Итог обработчика события: ошибка лишь пишется в лог.
pub trait EventOutcome: Send {
    fn log(self, topic: Topic);
}

impl EventOutcome for () {
    fn log(self, _: Topic) {}
}

impl<T: Send, E: std::fmt::Display + Send> EventOutcome for Result<T, E> {
    fn log(self, topic: Topic) {
        if let Err(e) = self {
            log::warn!(topic:% = topic, error:% = e; "event handler failed");
        }
    }
}

/// Модуль до подключения: имя, surface, capability и обработчики.
pub struct Module {
    name: ModuleId,
    version: String,
    about: Option<String>,
    surface: Option<Surface>,
    groups: Vec<GroupDecl>,
    tools: Vec<ToolEntry>,
    events: Vec<(Topic, EventFn)>,
    hooks: HookSlots,
    hook_points: Vec<HookPoint>,
    hooks_timeout: Option<HumanDuration>,
    fail_mode: FailMode,
    provider: Option<Arc<dyn DynProvider>>,
    options: ClientOptions,
    connect_wait: Duration,
    backoff: (Duration, Duration),
}

impl Module {
    pub fn new(name: impl Into<ModuleId>) -> Self {
        Self {
            name: name.into(),
            version: String::from("0.0.0"),
            about: None,
            surface: None,
            groups: Vec::new(),
            tools: Vec::new(),
            events: Vec::new(),
            hooks: HookSlots::default(),
            hook_points: Vec::new(),
            hooks_timeout: None,
            fail_mode: FailMode::default(),
            provider: None,
            options: ClientOptions::default(),
            connect_wait: DEFAULT_CONNECT_WAIT,
            backoff: (backoff::MIN, backoff::MAX),
        }
    }

    /// Версия модуля для `modules.list`.
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Описание для логов: в `hello` поля под него нет.
    pub fn about(mut self, about: impl Into<String>) -> Self {
        self.about = Some(about.into());
        self
    }

    pub fn surface(mut self, surface: Surface) -> Self {
        self.surface = Some(surface);
        self
    }

    pub fn group(mut self, path: impl Into<String>, about: impl Into<String>) -> Self {
        self.groups.push(GroupDecl {
            path: path.into(),
            about: about.into(),
        });
        self
    }

    /// Тул по пути `group.sub.tool`; вход — `#[data]`-тип, схема из его
    /// `JsonSchema`, описание по умолчанию — doc-комментарий типа.
    ///
    /// # Panics
    ///
    /// Путь уже занят: ошибка сборки модуля, а не данных.
    pub fn tool<I, E, F, Fut>(mut self, path: impl Into<String>, handler: F) -> Self
    where
        I: DeserializeOwned + JsonSchema + Send + 'static,
        E: Into<ToolError> + 'static,
        F: Fn(ToolCtx, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ToolResult, E>> + Send + 'static,
    {
        let path = path.into();
        assert!(
            self.tools.iter().all(|t| t.decl.path != path),
            "tool {path} is declared twice"
        );
        let mut schema = tool::input_schema::<I>();
        let description = schema
            .as_object_mut()
            .and_then(|s| s.remove("description"))
            .and_then(|d| d.as_str().map(str::to_owned))
            .unwrap_or_default();
        let run = tool::erase(schema.clone(), handler);
        self.tools.push(ToolEntry {
            decl: ToolDecl {
                path,
                description,
                input: schema,
                master_only: false,
                trusted_only: false,
                needs: Vec::new(),
                ends_turn: false,
                parallel_safe: false,
                waits_on_cancel: false,
                result: ResultForm::default(),
                timeout: None,
                idempotent: false,
            },
            run,
        });
        self
    }

    /// Правка последнего объявленного тула.
    ///
    /// # Panics
    ///
    /// До первого `.tool()`: ошибка сборки модуля.
    fn last_tool(&mut self, f: impl FnOnce(&mut ToolDecl)) {
        let tool = self
            .tools
            .last_mut()
            .expect("tool modifiers must follow .tool()");
        f(&mut tool.decl);
    }

    pub fn description(mut self, text: impl Into<String>) -> Self {
        let text = text.into();
        self.last_tool(|t| t.description = text);
        self
    }

    pub fn ends_turn(mut self, on: bool) -> Self {
        self.last_tool(|t| t.ends_turn = on);
        self
    }

    pub fn master_only(mut self, on: bool) -> Self {
        self.last_tool(|t| t.master_only = on);
        self
    }

    pub fn trusted_only(mut self, on: bool) -> Self {
        self.last_tool(|t| t.trusted_only = on);
        self
    }

    pub fn needs(mut self, needs: &[Need]) -> Self {
        self.last_tool(|t| t.needs = needs.to_vec());
        self
    }

    pub fn parallel_safe(mut self, on: bool) -> Self {
        self.last_tool(|t| t.parallel_safe = on);
        self
    }

    /// Повтор после реконнекта без `hello.resume` безопасен (`06 §8`).
    pub fn idempotent(mut self, on: bool) -> Self {
        self.last_tool(|t| t.idempotent = on);
        self
    }

    pub fn timeout(mut self, timeout: HumanDuration) -> Self {
        self.last_tool(|t| t.timeout = Some(timeout));
        self
    }

    /// Подписка на топик; ядро рассылает события уже в M1.
    pub fn on_event<F, Fut>(mut self, topic: Topic, handler: F) -> Self
    where
        F: Fn(Kernel, Event) -> Fut + Send + Sync + 'static,
        Fut: Future<Output: EventOutcome> + Send + 'static,
    {
        let handler = Arc::new(handler);
        self.events.push((
            topic,
            Arc::new(move |kernel, event| {
                let handler = handler.clone();
                Box::pin(async move { handler(kernel, event).await.log(topic) })
            }),
        ));
        self
    }

    /// Хук точки `point`; тип входа задаёт замыкание (`BeforeSend` и т.д.).
    ///
    /// # Panics
    ///
    /// Точка не совпала с типом входа или объявлена дважды: ошибка сборки модуля.
    pub fn hook<H, F, Fut>(mut self, point: HookPoint, handler: F) -> Self
    where
        H: HookInput,
        H::Patch: serde::Serialize + DeserializeOwned + Send + 'static,
        F: Fn(Kernel, H) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Decision<H::Patch>> + Send + 'static,
    {
        assert_eq!(point, H::POINT, "hook handler input does not match {point}");
        assert!(
            !self.hook_points.contains(&point),
            "hook {point} is declared twice"
        );
        let handler = Arc::new(handler);
        *H::slot_mut(&mut self.hooks) = Some(Arc::new(move |kernel, input| {
            let handler = handler.clone();
            Box::pin(async move { handler(kernel, input).await })
        }));
        self.hook_points.push(point);
        self
    }

    /// Потолок ответа хука; ядро режет его до `modules.hooks.timeout`.
    pub fn hooks_timeout(mut self, timeout: HumanDuration) -> Self {
        self.hooks_timeout = Some(timeout);
        self
    }

    pub fn hooks_fail_mode(mut self, mode: FailMode) -> Self {
        self.fail_mode = mode;
        self
    }

    /// Запасной вход провайдера (`06 §3`): ядро примет его, только если
    /// модуль назван в `llm.providers` с `kind: module`.
    pub fn provider(mut self, provider: impl Provider + 'static) -> Self {
        self.provider = Some(Arc::new(provider));
        self
    }

    /// Транспорт, TLS и счётчик id.
    pub fn options(mut self, options: ClientOptions) -> Self {
        self.options = options;
        self
    }

    /// Сколько ждать ядро при первом подключении.
    pub fn connect_wait(mut self, wait: Duration) -> Self {
        self.connect_wait = wait;
        self
    }

    /// Пауза между попытками: от `min`, вдвое до `max` (`1s..60s`).
    pub fn reconnect_backoff(mut self, min: Duration, max: Duration) -> Self {
        self.backoff = (min, max);
        self
    }

    /// Capability для `hello`: секция только у объявленного.
    pub fn capabilities(&self) -> Capabilities {
        let mut subscriptions = Vec::new();
        for (topic, _) in &self.events {
            if !subscriptions.contains(topic) {
                subscriptions.push(*topic);
            }
        }

        Capabilities {
            provider: self.provider.as_ref().map(|p| ProviderCap {
                decl: p.decl(),
                models: p.models(),
            }),
            tools: (!self.tools.is_empty() || !self.groups.is_empty()).then(|| ToolsCap {
                groups: self.groups.clone(),
                tools: self.tools.iter().map(|t| t.decl.clone()).collect(),
            }),
            events: (!subscriptions.is_empty()).then_some(EventsCap { subscriptions }),
            hooks: (!self.hook_points.is_empty()).then(|| HooksCap {
                points: self.hook_points.clone(),
                timeout: self.hooks_timeout,
                fail_mode: self.fail_mode,
            }),
        }
    }

    /// Подключение по конфигу секции `module`.
    pub async fn connect_config(self, cfg: &ModuleConfig) -> Result<Connected, ConnectError> {
        let options = ClientOptions {
            config: cfg.transport.clone(),
            tls: cfg.tls.clone(),
            ids: self.options.ids.clone(),
        };
        self.options(options)
            .connect_wait(*cfg.connect_wait)
            .connect(&cfg.kernel, cfg.token.expose())
            .await
    }

    /// Ждёт ядро до `connect_wait`: обрыв транспорта — повтор с паузой,
    /// отказ `hello` по токену или версии — сразу ошибка. Возвращает после
    /// `welcome` и регистрации surface.
    pub async fn connect(self, target: &Url, token: &str) -> Result<Connected, ConnectError> {
        let deadline = Instant::now() + self.connect_wait;
        let runtime = self.into_runtime(Some(target.clone()), token);
        let mut backoff = runtime.backoff();

        loop {
            match runtime.establish(None, runtime.options.ids.clone()).await {
                Ok(()) => return Ok(Connected { runtime }),
                Err(e) if fatal(&e) => return Err(e),
                Err(e) => {
                    let delay = backoff.next_delay();
                    if Instant::now() + delay > deadline {
                        return Err(e);
                    }
                    log::warn!(module:% = runtime.name, error:% = e, retry_in:? = delay; "kernel is not reachable yet");
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// Handshake поверх готового соединения (`Conn::pair` в тестах);
    /// без адреса `run` не переподключается.
    pub async fn handshake(self, conn: Conn, token: &str) -> Result<Connected, ConnectError> {
        let runtime = self.into_runtime(None, token);
        let ids = runtime.options.ids.clone();
        runtime.establish_over(conn, None, ids).await?;
        Ok(Connected { runtime })
    }

    fn into_runtime(self, target: Option<Url>, token: &str) -> Arc<Runtime> {
        let capabilities = self.capabilities();
        let Self {
            name,
            version,
            about,
            surface,
            tools,
            events,
            hooks,
            provider,
            options,
            backoff,
            ..
        } = self;

        let slot = Arc::new(RwLock::new(None));
        let tools: BTreeMap<_, _> = tools
            .into_iter()
            .map(|t| (t.decl.path.clone(), t))
            .collect();
        let registry = Arc::new(Registry::new(
            tools,
            events,
            hooks,
            provider,
            Arc::downgrade(&slot),
        ));

        Arc::new(Runtime {
            name,
            version,
            about,
            token: token.to_owned(),
            target,
            capabilities,
            surface,
            options,
            backoff,
            registry,
            slot,
        })
    }
}

/// Всё, что нужно для (пере)подключения.
struct Runtime {
    name: ModuleId,
    version: String,
    about: Option<String>,
    token: String,
    target: Option<Url>,
    capabilities: Capabilities,
    surface: Option<Surface>,
    options: ClientOptions,
    backoff: (Duration, Duration),
    registry: Arc<Registry>,
    slot: Arc<Slot>,
}

impl Runtime {
    fn backoff(&self) -> Backoff {
        Backoff::new(self.backoff.0, self.backoff.1)
    }

    fn hello(&self, resume: Option<SessionId>) -> Hello {
        Hello {
            module: self.name.clone(),
            version: self.version.clone(),
            proto_version: ProtoVersion::CURRENT,
            token: self.token.as_str().into(),
            capabilities: self.capabilities.clone(),
            resume: resume.map(|session_id| Resume { session_id }),
        }
    }

    async fn establish(&self, resume: Option<SessionId>, ids: IdSeq) -> Result<(), ConnectError> {
        let Some(target) = &self.target else {
            return Err(ConnectError::Transport(
                "the module was connected without an address".to_owned(),
            ));
        };
        let conn_state = Arc::new(ConnState::default());
        let handler = SdkHandler::new(self.registry.clone(), conn_state.clone());
        let options = ClientOptions {
            ids,
            ..self.options.clone()
        };
        let (client, welcome) =
            RpcClient::connect_with(target, self.hello(resume), options, handler).await?;
        self.install(client, welcome, conn_state).await
    }

    async fn establish_over(
        &self,
        conn: Conn,
        resume: Option<SessionId>,
        ids: IdSeq,
    ) -> Result<(), ConnectError> {
        let conn_state = Arc::new(ConnState::default());
        let handler = SdkHandler::new(self.registry.clone(), conn_state.clone());
        let options = ClientOptions {
            ids,
            ..self.options.clone()
        };
        let (client, welcome) =
            RpcClient::handshake(conn, self.hello(resume), options, handler).await?;
        self.install(client, welcome, conn_state).await
    }

    /// Новое соединение — текущее; затем surface. Отказ регистрации
    /// закрывает соединение: модуль без своего surface бесполезен.
    async fn install(
        &self,
        client: RpcClient,
        welcome: Welcome,
        conn: Arc<ConnState>,
    ) -> Result<(), ConnectError> {
        conn.bind(&client, &welcome);
        for warning in &welcome.warnings {
            log::warn!(module:% = self.name, subject = warning.subject.as_str(), code:% = warning.code, message = warning.message.as_str(); "kernel rejected part of hello");
        }

        let mut kernel = client.session();
        let previous = self
            .slot
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(Current {
                client: client.clone(),
                welcome: welcome.clone(),
            });
        drop(previous);

        if let Some(surface) = &self.surface
            && let Err(error) = surface.register(&mut kernel).await
        {
            client.peer().close(RpcError::new(
                ErrorCode::Cancelled,
                "surface registration failed",
            ));
            return Err(ConnectError::Rejected(error));
        }

        log::info!(
            module:% = self.name,
            session:% = welcome.session_id,
            kind:% = welcome.kind,
            about = self.about.as_deref().unwrap_or("");
            "connected to the kernel"
        );
        Ok(())
    }

    fn current<T>(&self, f: impl FnOnce(&Current) -> T) -> Option<T> {
        self.slot
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(f)
    }
}

/// Ошибка, после которой повтор бесполезен: ядро отказало по существу.
fn fatal(error: &ConnectError) -> bool {
    match error {
        ConnectError::Transport(_) | ConnectError::Handshake(_) => false,
        ConnectError::Rejected(error) => !matches!(
            error.code,
            ErrorCode::Unavailable
                | ErrorCode::Timeout
                | ErrorCode::Cancelled
                | ErrorCode::Internal
                | ErrorCode::Quota
                | ErrorCode::Unknown
        ),
    }
}

/// Подключённый модуль; клоны делят одно подключение — `run` съедает один из них.
#[derive(Clone)]
pub struct Connected {
    runtime: Arc<Runtime>,
}

impl Connected {
    /// Сессия текущего соединения; после реконнекта — уже нового.
    pub fn kernel(&self) -> Kernel {
        self.client().session()
    }

    /// # Panics
    ///
    /// Никогда: `Connected` существует только после первого `welcome`.
    pub fn client(&self) -> RpcClient {
        self.runtime
            .current(|c| c.client.clone())
            .expect("a connected module always has a current connection")
    }

    pub fn welcome(&self) -> Welcome {
        self.runtime
            .current(|c| c.welcome.clone())
            .expect("a connected module always has a current connection")
    }

    /// Caller, которого решил токен.
    pub fn caller(&self) -> Caller {
        self.client().caller().clone()
    }

    /// Живёт до `shutdown`: обрыв — реконнект с паузой `1s..60s` и
    /// `hello.resume`; отказ токена или версии и вытеснение другой сессией
    /// того же имени — ошибка.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> eyre::Result<()> {
        let runtime = self.runtime;
        let mut shutdown = pin!(shutdown);
        let mut backoff = runtime.backoff();

        loop {
            let Some((client, session)) =
                runtime.current(|c| (c.client.clone(), c.welcome.session_id.clone()))
            else {
                eyre::bail!("module {} has no connection", runtime.name);
            };

            let reason = tokio::select! {
                () = &mut shutdown => {
                    client.peer().close(RpcError::new(ErrorCode::Unavailable, "module is shutting down"));
                    return Ok(());
                }
                reason = client.closed() => reason,
            };
            // Вытеснение — другая живая сессия с этим именем: переподключение затеяло бы драку.
            if reason.code == ErrorCode::Conflict {
                eyre::bail!("module {} was superseded: {reason}", runtime.name);
            }
            if runtime.target.is_none() {
                eyre::bail!("connection to the kernel closed: {reason}");
            }
            log::warn!(module:% = runtime.name, reason:% = reason; "kernel connection lost, reconnecting");
            let ids = client.peer().ids().clone();
            drop(client);

            loop {
                let delay = backoff.next_delay();
                tokio::select! {
                    () = &mut shutdown => return Ok(()),
                    () = tokio::time::sleep(delay) => {}
                }
                let attempt = tokio::select! {
                    () = &mut shutdown => return Ok(()),
                    attempt = runtime.establish(Some(session.clone()), ids.clone()) => attempt,
                };
                match attempt {
                    Ok(()) => {
                        backoff.reset();
                        replay_attach(&runtime).await;
                        break;
                    }
                    Err(e) if fatal(&e) => {
                        return Err(eyre::eyre!("module {} cannot reconnect: {e}", runtime.name));
                    }
                    Err(e) => {
                        log::warn!(module:% = runtime.name, error:% = e; "reconnect failed");
                    }
                }
            }
        }
    }
}

/// M2: после реконнекта `turns.attach{chat, since_seq}` по каждому чату из
/// `chats.list{live: true}` (`06 §8`); пока ядро отвечает на них `M2_PENDING`.
async fn replay_attach(_runtime: &Runtime) {}

impl std::fmt::Debug for Connected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connected")
            .field("module", &self.runtime.name)
            .field(
                "session",
                &self.runtime.current(|c| c.welcome.session_id.clone()),
            )
            .finish_non_exhaustive()
    }
}
