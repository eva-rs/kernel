//! `Module` — билдер модуля; сервис из него — `Listening` (`listen`),
//! клиент — `Connected` с реконнектом (`06 §8`, `§10`).

use std::{collections::BTreeMap, pin::pin, sync::Arc, time::Duration};

use eva::{_priv::schemars::JsonSchema, time::HumanDuration, url::Url, utils::SecretString};
use serde::de::DeserializeOwned;
use tokio::time::Instant;

use eva_core::{Caller, Provider};
use eva_protocol::{
    types::{
        ModuleId, SessionId,
        tool::{GroupDecl, Need, ResultForm},
    },
    wire::{
        Capabilities, ErrorCode, Hello, RpcError, Welcome,
        capabilities::{EventsCap, FailMode, HooksCap, ProviderCap, ToolsCap},
        events::{Event, Topic},
        handshake::{ProtoVersion, Resume},
        hooks::{Decision, HookPoint},
    },
};
use eva_rpc::{ClientOptions, ClientTls, Conn, ConnectError, IdSeq, Psk, RpcClient};

use crate::{
    Soul,
    app::Initialization,
    backoff::{self, Backoff},
    config::{ClientConfig, DEFAULT_WAIT, ServiceConfig, read_token},
    group::{Description, Group, IntoToolResult, Tool},
    handler::{ConnState, DynProvider, EventFn, HookInput, HookSlots, Registry, SdkHandler},
    listen::{self, Listening},
    live::{Current, Link},
    tool::{self, ToolCtx, ToolError},
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

/// Билдер модуля с общим состоянием обработчиков. Имя задаёт дефолты приложения;
/// авторизованный id приходит от soul в `welcome`.
pub struct Module<S = ()> {
    name: String,
    translations: Arc<crate::Catalogs>,
    pub(crate) state: Option<Arc<S>>,
    pub(crate) initialization: Option<Initialization<S>>,
    version: String,
    about: Option<String>,
    ask: bool,
    groups: Vec<GroupDecl>,
    tools: Vec<Tool<S>>,
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

impl Default for Module {
    fn default() -> Self {
        Self {
            name: String::from("module"),
            translations: Arc::new(crate::Catalogs::default()),
            state: Some(Arc::new(())),
            initialization: None,
            version: String::from("0.0.0"),
            about: None,
            ask: false,
            groups: Vec::new(),
            tools: Vec::new(),
            events: Vec::new(),
            hooks: HookSlots::default(),
            hook_points: Vec::new(),
            hooks_timeout: None,
            fail_mode: FailMode::default(),
            provider: None,
            options: ClientOptions::default(),
            connect_wait: DEFAULT_WAIT,
            backoff: (backoff::MIN, backoff::MAX),
        }
    }
}

impl Module {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state<S: Send + Sync + 'static>(self, state: S) -> Module<S> {
        let mut module = self.retype();
        module.state = Some(Arc::new(state));
        module
    }

    pub(crate) fn retype<S>(self) -> Module<S> {
        assert!(
            self.tools.is_empty(),
            "set module state before registering tools"
        );
        assert!(
            self.initialization.is_none(),
            "module initialization is already configured"
        );
        Module {
            translations: self.translations,
            name: self.name,
            version: self.version,
            about: self.about,
            ask: self.ask,
            groups: self.groups,
            tools: Vec::new(),
            events: self.events,
            hooks: self.hooks,
            hook_points: self.hook_points,
            hooks_timeout: self.hooks_timeout,
            fail_mode: self.fail_mode,
            provider: self.provider,
            options: self.options,
            connect_wait: self.connect_wait,
            backoff: self.backoff,
            state: None,
            initialization: None,
        }
    }
}

impl<S: Send + Sync + 'static> Module<S> {
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn translations(mut self, catalogs: crate::Catalogs) -> Self {
        self.translations = Arc::new(catalogs);
        self
    }

    pub fn module_name(&self) -> &str {
        &self.name
    }
    pub fn module_version(&self) -> &str {
        &self.version
    }

    pub fn group(mut self, group: Group<S>) -> Self {
        group.flatten("", &mut self.groups, &mut self.tools);
        self.check_tools();
        self
    }

    pub fn tool(mut self, tool: Tool<S>) -> Self {
        self.tools.push(tool);
        self.check_tools();
        self
    }

    fn check_tools(&self) {
        let mut paths = std::collections::BTreeSet::new();
        let mut names = std::collections::BTreeSet::new();
        for tool in &self.tools {
            let path = &tool.path;
            assert!(paths.insert(path), "tool {path} is declared twice");
            assert!(
                names.insert(tool::canonical(path)),
                "canonical tool name collision: {path}"
            );
        }
    }

    /// Версия модуля для списка подключений.
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Описание для логов: в `hello` поля под него нет.
    pub fn about(mut self, about: impl Into<String>) -> Self {
        self.about = Some(about.into());
        self
    }

    /// Capability `ask`: модуль рисует кнопки pending-запросов в чатах surface,
    /// которые обслуживает (свои, клиенту — ещё `local`).
    pub fn ask(mut self, ask: bool) -> Self {
        self.ask = ask;
        self
    }

    pub fn group_path(mut self, path: impl Into<String>, about: impl Into<String>) -> Self {
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
    pub fn tool_path<I, O, E, F, Fut>(self, path: impl Into<String>, handler: F) -> Self
    where
        I: DeserializeOwned + JsonSchema + Send + 'static,
        O: IntoToolResult + Send + 'static,
        E: Into<ToolError> + 'static,
        F: Fn(ToolCtx<S>, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, E>> + Send + 'static,
    {
        self.tool(Tool::at_path(path.into(), handler))
    }

    /// Правка последнего объявленного тула.
    ///
    /// # Panics
    ///
    /// До первого `.tool()`: ошибка сборки модуля.
    fn last_tool(mut self, f: impl FnOnce(Tool<S>) -> Tool<S>) -> Self {
        let tool = self
            .tools
            .pop()
            .expect("tool modifiers must follow .tool()");
        self.tools.push(f(tool));
        self
    }

    pub fn describe(self, description: Description) -> Self {
        self.last_tool(|tool| tool.describe(description))
    }

    pub fn description(self, text: impl Into<String>) -> Self {
        self.last_tool(|tool| tool.description(text))
    }

    pub fn ends_turn(self, on: bool) -> Self {
        self.last_tool(|tool| tool.ends_turn(on))
    }

    pub fn master_only(self, on: bool) -> Self {
        self.last_tool(|tool| tool.master_only(on))
    }

    pub fn trusted_only(self, on: bool) -> Self {
        self.last_tool(|tool| tool.trusted_only(on))
    }

    pub fn needs(self, needs: &[Need]) -> Self {
        self.last_tool(|tool| tool.needs(needs))
    }

    pub fn parallel_safe(self, on: bool) -> Self {
        self.last_tool(|tool| tool.parallel_safe(on))
    }

    pub fn idempotent(self, on: bool) -> Self {
        self.last_tool(|tool| tool.idempotent(on))
    }

    pub fn waits_on_cancel(self, on: bool) -> Self {
        self.last_tool(|tool| tool.waits_on_cancel(on))
    }

    pub fn result(self, result: ResultForm) -> Self {
        self.last_tool(|tool| tool.result(result))
    }

    pub fn timeout(self, timeout: HumanDuration) -> Self {
        self.last_tool(|tool| tool.timeout(timeout))
    }

    /// Подписка на топик; soul рассылает события уже в M1.
    pub fn on_event<F, Fut>(mut self, topic: Topic, handler: F) -> Self
    where
        F: Fn(Soul, Event) -> Fut + Send + Sync + 'static,
        Fut: Future<Output: EventOutcome> + Send + 'static,
    {
        let handler = Arc::new(handler);
        self.events.push((
            topic,
            Arc::new(move |soul, event| {
                let handler = handler.clone();
                Box::pin(async move { handler(soul, event).await.log(topic) })
            }),
        ));
        self
    }

    /// Хук точки `point`; тип входа задаёт замыкание (`AfterTool` и т. д.).
    ///
    /// # Panics
    ///
    /// Точка не совпала с типом входа или объявлена дважды: ошибка сборки модуля.
    pub fn hook<H, F, Fut>(mut self, point: HookPoint, handler: F) -> Self
    where
        H: HookInput,
        H::Patch: serde::Serialize + DeserializeOwned + Send + 'static,
        F: Fn(Soul, H) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Decision<H::Patch>> + Send + 'static,
    {
        assert_eq!(point, H::POINT, "hook handler input does not match {point}");
        assert!(
            !self.hook_points.contains(&point),
            "hook {point} is declared twice"
        );
        let handler = Arc::new(handler);
        *H::slot_mut(&mut self.hooks) = Some(Arc::new(move |soul, input| {
            let handler = handler.clone();
            Box::pin(async move { handler(soul, input).await })
        }));
        self.hook_points.push(point);
        self
    }

    /// Потолок ответа хука; soul режет его до `modules.hooks.timeout`.
    pub fn hooks_timeout(mut self, timeout: HumanDuration) -> Self {
        self.hooks_timeout = Some(timeout);
        self
    }

    pub fn hooks_fail_mode(mut self, mode: FailMode) -> Self {
        self.fail_mode = mode;
        self
    }

    /// Запасной вход провайдера (`06 §3`): soul примет его, только если
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

    /// Клиент: сколько ждать soul при первом подключении.
    pub fn connect_wait(mut self, wait: Duration) -> Self {
        self.connect_wait = wait;
        self
    }

    /// Клиент: пауза между попытками, от `min` вдвое до `max` (`1s..60s`).
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
                tools: self.tools.iter().map(Tool::decl).collect(),
            }),
            events: (!subscriptions.is_empty()).then_some(EventsCap { subscriptions }),
            hooks: (!self.hook_points.is_empty()).then(|| HooksCap {
                points: self.hook_points.clone(),
                timeout: self.hooks_timeout,
                fail_mode: self.fail_mode,
            }),
            ask: self.ask,
        }
    }

    /// Сервис: слушает `module.listen` (или сокет от менеджера служб) и
    /// возвращается сразу после bind; соединения от soul принимаются в фоне.
    /// Нечитаемый или пустой `module.key` и занятый адрес — ошибка.
    pub async fn listen(self, cfg: &ServiceConfig) -> Result<Listening, ConnectError> {
        let psk =
            Psk::read(&cfg.key).map_err(|e| ConnectError::Config(format!("module.key: {e}")))?;
        let options = ClientOptions {
            config: cfg.transport.clone(),
            tls: ClientTls::default(),
            ids: self.options.ids.clone(),
        };
        let runtime = self.options(options).into_runtime(None, None)?;
        let listener = listen::bind(&cfg.listen, psk, &cfg.transport).await?;
        Ok(Listening::start(
            runtime,
            listener,
            *cfg.transport.handshake_timeout,
            *cfg.soul_wait,
        ))
    }

    /// Клиент: подключение по конфигу секции `module`.
    pub async fn connect_config(self, cfg: &ClientConfig) -> Result<Connected, ConnectError> {
        let token = read_token(&cfg.token, "module.token")?;
        let options = ClientOptions {
            config: cfg.transport.clone(),
            tls: cfg.tls.clone(),
            ids: self.options.ids.clone(),
        };
        self.options(options)
            .connect_wait(*cfg.connect_wait)
            .connect_with_token(&cfg.soul, token)
            .await
    }

    /// Ждёт подключения к soul до `connect_wait`: обрыв транспорта — повтор с паузой,
    /// отказ `hello` по токену или версии — сразу ошибка. Возвращает после
    /// `welcome`; surface модуль создаёт сам (`soul().surfaces().create()`).
    pub async fn connect(
        self,
        target: &Url,
        token: &SecretString,
    ) -> Result<Connected, ConnectError> {
        let token = read_token(token, "token")?;
        self.connect_with_token(target, token).await
    }

    async fn connect_with_token(
        self,
        target: &Url,
        token: String,
    ) -> Result<Connected, ConnectError> {
        let deadline = Instant::now() + self.connect_wait;
        let runtime = self.into_runtime(Some(target.clone()), Some(token))?;
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
                    log::warn!(error:% = e, retry_in:? = delay; "soul is not reachable yet");
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    /// Handshake клиента поверх готового соединения (`Conn::pair` в тестах);
    /// без адреса `run` не переподключается.
    pub async fn handshake(
        self,
        conn: Conn,
        token: &SecretString,
    ) -> Result<Connected, ConnectError> {
        let token = read_token(token, "token")?;
        let runtime = self.into_runtime(None, Some(token))?;
        let ids = runtime.options.ids.clone();
        runtime.establish_over(conn, None, ids).await?;
        Ok(Connected { runtime })
    }

    fn into_runtime(
        self,
        target: Option<Url>,
        token: Option<String>,
    ) -> Result<Arc<Runtime>, ConnectError> {
        let state = self.state.clone().ok_or_else(|| {
            ConnectError::Config(
                "module state is not initialized; use .run() or .state(value)".into(),
            )
        })?;
        let capabilities = self.capabilities();
        let translations = self.translations.clone();
        let domain: Arc<str> = self.name.clone().into();
        let Self {
            version,
            about,
            tools,
            events,
            hooks,
            provider,
            options,
            backoff,
            ..
        } = self;

        let link = Arc::new(Link::new());
        let tools: BTreeMap<_, _> = tools
            .into_iter()
            .map(|t| {
                (
                    t.path.clone(),
                    t.bind(state.clone(), translations.clone(), domain.clone()),
                )
            })
            .collect();
        let registry = Arc::new(Registry::new(
            tools,
            events,
            hooks,
            provider,
            Arc::downgrade(&link),
        ));

        Ok(Arc::new(Runtime {
            version,
            about,
            token,
            target,
            capabilities,
            options,
            backoff,
            registry,
            link,
        }))
    }
}

/// Всё, что нужно для (пере)подключения.
pub(crate) struct Runtime {
    version: String,
    about: Option<String>,
    /// Только у клиента; прочитан из секрета один раз: реконнект файл не перечитывает.
    token: Option<String>,
    /// Адрес soul для клиента; без него `run` не переподключается.
    target: Option<Url>,
    capabilities: Capabilities,
    options: ClientOptions,
    backoff: (Duration, Duration),
    registry: Arc<Registry>,
    pub(crate) link: Arc<Link>,
}

impl Runtime {
    fn backoff(&self) -> Backoff {
        Backoff::new(self.backoff.0, self.backoff.1)
    }

    pub(crate) fn about(&self) -> &str {
        self.about.as_deref().unwrap_or("")
    }

    fn hello(&self, resume: Option<SessionId>) -> Hello {
        Hello {
            version: self.version.clone(),
            proto_version: ProtoVersion::CURRENT,
            token: self.token.as_deref().map(Into::into),
            capabilities: self.capabilities.clone(),
            resume: resume.map(|session_id| Resume { session_id }),
        }
    }

    /// Обработчик и опции нового соединения.
    fn prepare(&self, ids: IdSeq) -> (Arc<ConnState>, SdkHandler, ClientOptions) {
        let conn_state = Arc::new(ConnState::default());
        let handler = SdkHandler::new(self.registry.clone(), conn_state.clone());
        let options = ClientOptions {
            ids,
            ..self.options.clone()
        };
        (conn_state, handler, options)
    }

    async fn establish(&self, resume: Option<SessionId>, ids: IdSeq) -> Result<(), ConnectError> {
        let Some(target) = &self.target else {
            return Err(ConnectError::Transport(
                "the module was connected without an address".to_owned(),
            ));
        };
        let (conn_state, handler, options) = self.prepare(ids);
        let (client, welcome) =
            RpcClient::connect_with(target, self.hello(resume), options, handler).await?;
        // Прежнее соединение клиента уже закрыто: реконнект идёт после обрыва.
        drop(self.install(client, welcome, &conn_state));
        Ok(())
    }

    async fn establish_over(
        &self,
        conn: Conn,
        resume: Option<SessionId>,
        ids: IdSeq,
    ) -> Result<(), ConnectError> {
        let (conn_state, handler, options) = self.prepare(ids);
        let (client, welcome) =
            RpcClient::handshake(conn, self.hello(resume), options, handler).await?;
        drop(self.install(client, welcome, &conn_state));
        Ok(())
    }

    /// Сервис: `hello` без токена на соединении, которое soul набрал сам.
    /// `resume` и счётчик id — от текущего соединения процесса, если оно было;
    /// прежнее соединение закрывается `conflict "superseded"` (`06 §2`).
    pub(crate) async fn answer(&self, conn: Conn) -> Result<RpcClient, ConnectError> {
        let (resume, ids) = self
            .current(|c| {
                (
                    Some(c.welcome.session_id.clone()),
                    c.client.peer().ids().clone(),
                )
            })
            .unwrap_or_else(|| (None, self.options.ids.clone()));
        let (conn_state, handler, options) = self.prepare(ids);
        let (client, welcome) =
            RpcClient::answer(conn, self.hello(resume), options, handler).await?;
        if let Some(previous) = self.install(client.clone(), welcome, &conn_state) {
            previous
                .client
                .peer()
                .close(RpcError::new(ErrorCode::Conflict, "superseded"));
        }
        Ok(client)
    }

    /// Новое соединение — текущее, прежнее отдаётся вызывающему. Soul сам
    /// выводит обслуживание surface из сессии, поэтому после `welcome` модулю
    /// регистрировать нечего.
    fn install(
        &self,
        client: RpcClient,
        welcome: Welcome,
        conn: &Arc<ConnState>,
    ) -> Option<Current> {
        conn.bind(&client, &welcome);
        for warning in &welcome.warnings {
            log::warn!(module:% = welcome.module, subject = warning.subject.as_str(), code:% = warning.code, message = warning.message.as_str(); "soul rejected part of hello");
        }
        log::info!(
            module:% = welcome.module,
            session:% = welcome.session_id,
            kind:% = welcome.kind,
            resumed = welcome.resumed,
            about = self.about();
            "connected to the soul"
        );
        self.link.install(Current { client, welcome })
    }

    pub(crate) fn current<T>(&self, f: impl FnOnce(&Current) -> T) -> Option<T> {
        self.link.current(f)
    }

    /// Id модуля из последнего `welcome`.
    pub(crate) fn module(&self) -> Option<ModuleId> {
        self.current(|c| c.welcome.module.clone())
    }
}

/// Ошибка, после которой повтор бесполезен: soul отказал по существу.
fn fatal(error: &ConnectError) -> bool {
    match error {
        ConnectError::Transport(_) | ConnectError::Handshake(_) => false,
        ConnectError::Config(_) => true,
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
    /// Soul через текущее соединение: ручка, взятая до реконнекта, после
    /// него ведёт в новую сессию (`live`).
    ///
    /// # Panics
    ///
    /// Никогда: `Connected` существует только после первого `welcome`.
    pub fn soul(&self) -> Soul {
        self.runtime
            .link
            .soul()
            .expect("a connected module always has a current connection")
    }

    /// Id модуля из `welcome`: имя записи его токена у soul (`06 §2`).
    ///
    /// # Panics
    ///
    /// Никогда: `Connected` существует только после первого `welcome`.
    pub fn module(&self) -> ModuleId {
        self.runtime
            .module()
            .expect("a connected module always has a current connection")
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
    /// того же модуля — ошибка.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> eyre::Result<()> {
        let runtime = self.runtime;
        let mut shutdown = pin!(shutdown);
        let mut backoff = runtime.backoff();
        let _reconnecting = runtime
            .target
            .is_some()
            .then(|| runtime.link.reconnecting());

        loop {
            let Some((client, session, module)) = runtime.current(|c| {
                (
                    c.client.clone(),
                    c.welcome.session_id.clone(),
                    c.welcome.module.clone(),
                )
            }) else {
                eyre::bail!("the module has no connection");
            };

            let reason = tokio::select! {
                () = &mut shutdown => {
                    runtime.link.close();
                    return Ok(());
                }
                reason = client.closed() => reason,
            };
            // Вытеснение — другая живая сессия с этим токеном: переподключение затеяло бы драку.
            if reason.code == ErrorCode::Conflict {
                eyre::bail!("module {module} was superseded: {reason}");
            }
            if runtime.target.is_none() {
                eyre::bail!("connection to the soul closed: {reason}");
            }
            log::warn!(module:% = module, reason:% = reason; "soul connection lost, reconnecting");
            let ids = client.peer().ids().clone();
            drop(client);

            loop {
                let delay = backoff.next_delay();
                tokio::select! {
                    () = &mut shutdown => {
                        runtime.link.close();
                        return Ok(());
                    }
                    () = tokio::time::sleep(delay) => {}
                }
                let attempt = tokio::select! {
                    () = &mut shutdown => {
                        runtime.link.close();
                        return Ok(());
                    }
                    attempt = runtime.establish(Some(session.clone()), ids.clone()) => attempt,
                };
                match attempt {
                    Ok(()) => {
                        backoff.reset();
                        break;
                    }
                    Err(e) if fatal(&e) => {
                        return Err(eyre::eyre!("module {module} cannot reconnect: {e}"));
                    }
                    Err(e) => {
                        log::warn!(module:% = module, error:% = e; "reconnect failed");
                    }
                }
            }
        }
    }
}

impl std::fmt::Debug for Connected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connected")
            .field("module", &self.runtime.module())
            .field(
                "session",
                &self.runtime.current(|c| c.welcome.session_id.clone()),
            )
            .finish_non_exhaustive()
    }
}
