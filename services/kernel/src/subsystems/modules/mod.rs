//! Компонент `modules` (`06`): петли дозвона сервисов реестра, слушатели
//! клиентов `modules.listen`, handshake, RPC-сервер над `Kernel<C>`, живые
//! сессии и их состояние — строка `modules`, провайдер запасной двери,
//! рассылка событий.
//!
//! Capability `tools` и `hooks` в M1 только проверяются и хранятся (строка
//! `modules`, `Entry::capabilities`): вызовы `tools.call` приходят с реестром
//! тулов (M4), `hooks.<point>` — с движком туров (M2).

use std::{path::Path, pin::pin, time::Duration};

use tokio::{
    sync::{broadcast, mpsc, watch},
    task::JoinSet,
    time::{Instant, MissedTickBehavior},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use eva_core::Caller;
use eva_protocol::{
    types::ModuleId,
    wire::{
        Capabilities, ErrorCode, RpcError,
        events::{Event, ModuleEvent},
    },
};
use eva_rpc::{Accepted, Inherited, Listener, Psk, Server, config::Listen};

use crate::{
    bus::Published,
    components::Components,
    config::ModulesConfig,
    convert,
    db::{Db as _, DbResult, Queries as _, stats::Metric},
    kernel::Kernel,
    shared::Shared,
    subsystems::modules::{
        dial::Dialer,
        live::{Attach, Entry, Source},
        services::{Control, Loop, Service, Services, trimmed},
        tokens::ConfigTokens,
    },
};

pub use self::auth::KernelAuth;

pub mod auth;
pub mod dial;
mod events;
pub mod live;
pub mod services;
pub mod tokens;

/// Сколько остановка ждёт, пока сессии приберут за собой.
const DRAIN: Duration = Duration::from_secs(5);

/// Как часто давно молчащие модули переводятся в `forgotten` (`06 §8`).
pub const FORGET_EVERY: Duration = Duration::from_secs(60 * 60);

/// Модули из конфига: токены клиентов и реестр сервисов (`06 §2`).
pub struct Registrations {
    pub tokens: ConfigTokens,
    pub services: Services,
}

impl Registrations {
    pub fn load(cfg: &ModulesConfig) -> eyre::Result<Self> {
        Ok(Self {
            tokens: ConfigTokens::load(&cfg.tokens)?,
            services: Services::load(&cfg.services, &cfg.tokens)?,
        })
    }

    /// Строка БД с этим id инертна: конфиг сильнее.
    pub fn shadowed(&self, module: &ModuleId) -> bool {
        self.services.shadowed(module, &self.tokens)
    }
}

/// Поднятые слушатели и петли дозвона; `shutdown` гасит их и все сессии.
pub struct Listening<C: Components> {
    shared: Shared<C>,
    stop: watch::Sender<bool>,
    dials: CancellationToken,
    tasks: JoinSet<()>,
    sessions: TaskTracker,
}

/// Слушатели `modules.listen` (относительный путь сокета — от
/// `system.state_dir`; сокет менеджера служб с тем же адресом берётся
/// готовым) и петли дозвона записей реестра.
pub async fn listen<C: Components>(kernel: &Kernel<C>) -> eyre::Result<Listening<C>> {
    let shared = kernel.shared().clone();
    let cfg = &shared.cfg().modules;
    let server = Server::new(
        kernel.clone(),
        KernelAuth::new(shared.clone()),
        cfg.transport.clone(),
    );
    let (stop, stopped) = watch::channel(false);
    let mut tasks = JoinSet::new();
    let sessions = TaskTracker::new();

    let mut inherited = Inherited::take().map_err(|e| eyre::eyre!("modules.listen: {e}"))?;
    for listen in &cfg.listen {
        let listen = resolve(listen, &shared.cfg().system.state_dir);
        let listener = Listener::open(&listen, &mut inherited, &cfg.transport)
            .await
            .map_err(|e| eyre::eyre!("failed to listen on {}: {e}", describe(&listen)))?;
        log::info!(address = describe(&listen), inherited = listener.inherited(); "modules listener bound");

        let server = server.clone();
        let session_shared = shared.clone();
        let session_tasks = sessions.clone();
        let mut stopped = stopped.clone();
        tasks.spawn(async move {
            let shutdown = async move {
                _ = stopped.wait_for(|stop| *stop).await;
            };
            server
                .serve(listener, shutdown, move |accepted| {
                    on_session(&session_shared, &session_tasks, accepted);
                })
                .await;
        });
    }
    inherited
        .finish()
        .map_err(|e| eyre::eyre!("modules.listen: {e}"))?;

    let dials = CancellationToken::new();
    let (control, commands) = mpsc::unbounded_channel();
    {
        // Под замком `modules.add/remove`: запись, добавленная между чтением
        // строк и подключением петель, не потеряется и не задвоится.
        let services = shared.services();
        let _ops = services.ops().await;
        let runtime = runtime_services(&shared).await?;
        services.attach(control.clone());
        let config = services.config().iter().map(|(module, service)| Loop {
            module: module.clone(),
            service: service.clone(),
            epoch: None,
        });
        for run in config.chain(runtime) {
            _ = control.send(Control::Start(run));
        }
    }
    tasks.spawn(
        Dialer {
            server,
            shared: shared.clone(),
            sessions: sessions.clone(),
            stop: dials.clone(),
        }
        .run(commands),
    );

    let mut stopped = stopped.clone();
    tasks.spawn(forget_every(shared.clone(), FORGET_EVERY, async move {
        _ = stopped.wait_for(|stop| *stop).await;
    }));

    Ok(Listening {
        shared,
        stop,
        dials,
        tasks,
        sessions,
    })
}

/// Строки `module_services`, не затенённые конфигом; битая строка и строка
/// с адресом записи конфига — в лог: конфиг сильнее.
async fn runtime_services<C: Components>(shared: &Shared<C>) -> eyre::Result<Vec<Loop>> {
    let rows = shared
        .db()
        .reader()
        .await?
        .run(|c| c.modules().dialable())
        .await?;
    let mut services = Vec::with_capacity(rows.len());
    for (row, key) in rows {
        if shared.services().shadowed(&row.module, shared.tokens()) {
            continue;
        }
        let address = match convert::module::service_address(&row) {
            Ok(address) => address,
            Err(e) => {
                log::error!(module:% = row.module, error:% = e; "module_services row is not dialable");
                continue;
            }
        };
        if let Some(twin) = shared.services().dialing(&address) {
            log::error!(module:% = row.module, config:% = twin, address:% = address; "module_services row dials the address of a modules.services entry, ignored");
            continue;
        }
        let service = Service {
            address,
            psk: Psk::derive(trimmed(key.expose())),
            source: Source::RuntimeService,
        };
        services.push(Loop {
            epoch: Some(shared.live().epoch(&row.module)),
            module: row.module,
            service,
        });
    }
    Ok(services)
}

impl<C: Components> Listening<C> {
    /// Петли дозвона и приём встают первыми, затем закрываются сессии:
    /// попытка в полёте после `shut_down` сессией уже не станет.
    pub async fn shutdown(mut self) {
        self.dials.cancel();
        _ = self.stop.send(true);
        self.shared.services().detach();
        let reason = RpcError::new(ErrorCode::Unavailable, "kernel is shutting down");
        self.shared.live().shut_down(&reason);
        while self.tasks.join_next().await.is_some() {}

        // Ждём сами задачи уборки, а не пустой реестр: запись `offline` должна
        // лечь до финального чекпойнта базы.
        self.sessions.close();
        let drained = tokio::time::timeout(DRAIN, self.sessions.wait()).await;
        if drained.is_err() {
            log::warn!(sessions:? = self.shared.live().sessions(); "module sessions did not close in time");
        }
    }
}

/// Компонент `Supervisor`: слушатели подняты до старта компонентов, чтобы
/// занятый сокет ронял процесс, а не одну подсистему.
pub async fn run<C: Components>(
    listening: Listening<C>,
    mut rx: eva::supervisor::SlaveRx,
) -> eyre::Result<()> {
    rx.recv().await;
    listening.shutdown().await;
    log::info!("modules stopped");
    Ok(())
}

/// `offline` дольше `modules.offline_forget` → `forgotten` (`06 §8`); сколько забыто.
pub async fn forget_offline<C: Components>(shared: &Shared<C>) -> DbResult<usize> {
    let (now, after) = (shared.now(), *shared.cfg().modules.offline_forget);
    shared
        .db()
        .write(move |tx| tx.modules().forget(now, after))
        .await
}

/// `forget_offline` раз в `every`, пока не придёт `stop`: ядро живёт
/// неделями, одного забывания на старте мало.
pub async fn forget_every<C: Components>(
    shared: Shared<C>,
    every: Duration,
    stop: impl Future<Output = ()>,
) {
    let mut stop = pin!(stop);
    let mut tick = tokio::time::interval_at(Instant::now() + every, every);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = &mut stop => break,
            _ = tick.tick() => match forget_offline(&shared).await {
                Ok(0) => {}
                Ok(forgotten) => log::info!(forgotten; "offline modules forgotten"),
                Err(e) => log::error!(error:% = e; "failed to forget offline modules"),
            },
        }
    }
}

/// `Policy` отказал вызову модуля: `modules.forbidden{module}` (`09 §5`).
pub async fn forbidden<C: Components>(shared: &Shared<C>, caller: &Caller) {
    if let Some(module) = caller.module() {
        shared.bump(Metric::ModulesForbidden, module.as_str()).await;
    }
}

fn resolve(listen: &Listen, state_dir: &Path) -> Listen {
    match listen {
        Listen::Uds(uds) if uds.path.is_relative() => {
            let mut uds = uds.clone();
            uds.path = state_dir.join(&uds.path);
            Listen::Uds(uds)
        }
        other => other.clone(),
    }
}

fn describe(listen: &Listen) -> String {
    match listen {
        Listen::Uds(uds) => format!("uds://{}", uds.path.display()),
        Listen::Ws(ws) => format!("ws://{}", ws.addr),
        Listen::Wss(wss) => format!("wss://{}", wss.addr),
    }
}

/// Сессия прошла handshake: пир в реестр живых, провайдер запасной двери —
/// в реестр моделей, подписка на события; уборка — по закрытию соединения.
pub(super) fn on_session<C: Components>(
    shared: &Shared<C>,
    sessions: &TaskTracker,
    accepted: Accepted,
) {
    let module = accepted.welcome.module.clone();
    let session = accepted.welcome.session_id.clone();
    let peer = accepted.peer.clone();

    match shared.live().attach(&module, &session, peer.clone()) {
        Attach::Superseded => {
            peer.close(superseded());
            return;
        }
        // Отозвана до `welcome`: ни провайдера, ни подписки — только уборка.
        Attach::Closing(reason) => peer.close(reason),
        Attach::Live(capabilities, bus) => serve(shared, &accepted, capabilities, bus),
    }

    let shared = shared.clone();
    sessions.spawn(async move {
        let reason = peer.closed().await;
        let Some(entry) = shared.live().release(&module, &session) else {
            return;
        };
        cleanup(&shared, &module, &entry).await;
        log::info!(module:% = module, session:% = session, reason:% = reason; "module session closed");
    });
}

/// Живая сессия: провайдер запасной двери, рассылка шины, `module.connected`.
fn serve<C: Components>(
    shared: &Shared<C>,
    accepted: &Accepted,
    capabilities: Capabilities,
    bus: Option<broadcast::Receiver<Published>>,
) {
    let module = &accepted.welcome.module;
    let accepted_models = &accepted.welcome.models_accepted;
    if let Some(mut cap) = capabilities.provider
        && !accepted_models.is_empty()
    {
        // Провайдер несёт только принятые модели: отказанное в `welcome` имя не перехватит и потом.
        cap.models.retain(|m| accepted_models.contains(&m.name));
        let provider = accepted.module().provider(cap);
        let models = shared.set_module_provider(module, Some(C::Provider::from(provider)));
        log::info!(module:% = module, models = models.iter().filter(|m| m.provider == module.as_str()).count(); "module provider registered");
    }

    if let (Some(events), Some(bus)) = (capabilities.events, bus) {
        events::forward(
            shared.clone(),
            accepted.caller.clone(),
            events.subscriptions.into_iter().collect(),
            accepted.module(),
            bus,
        );
    }

    shared.bus().publish(
        Event::ModuleConnected(ModuleEvent {
            module: module.clone(),
        }),
        None,
    );
}

fn superseded() -> RpcError {
    RpcError::new(ErrorCode::Conflict, "superseded")
}

/// Вытесненная сессия: закрыть; состояние модуля (строка `modules`) уже
/// принадлежит новой, поэтому прибирается только память сессии.
pub(crate) fn cleanup_superseded<C: Components>(
    shared: &Shared<C>,
    module: &ModuleId,
    mut old: Entry,
) {
    old.close(superseded());
    if old.capabilities.provider.is_some() {
        shared.set_module_provider(module, None);
    }
    log::info!(module:% = module, session:% = old.session; "module session superseded");
}

/// Текущая сессия модуля закрылась: модуль offline (`06 §8`).
async fn cleanup<C: Components>(shared: &Shared<C>, module: &ModuleId, entry: &Entry) {
    if entry.capabilities.provider.is_some() {
        shared.set_module_provider(module, None);
    }

    let (now, zone) = (shared.now(), shared.day_zone());
    let name = module.clone();
    if let Err(e) = shared
        .db()
        .write(move |tx| {
            tx.modules().disconnected(&name, now)?;
            tx.stats()
                .bump(Metric::ModulesDisconnected, name.as_str(), now, &zone)
        })
        .await
    {
        log::error!(module:% = module, error:% = e; "failed to mark module offline");
    }

    shared.bus().publish(
        Event::ModuleDisconnected(ModuleEvent {
            module: module.clone(),
        }),
        None,
    );
}
