//! Компонент `modules` (`06`): слушатели `modules.listen`, handshake,
//! RPC-сервер над `Kernel<C>`, живые сессии и их состояние — строка
//! `modules`, провайдер запасной двери, рассылка событий.
//!
//! Capability `tools` и `hooks` в M1 только проверяются и хранятся (строка
//! `modules`, `Entry::capabilities`): вызовы `tools.call` приходят с реестром
//! тулов (M4), `hooks.<point>` — с движком туров (M2).

use std::{path::Path, time::Duration};

use tokio::{sync::watch, task::JoinSet};

use eva_protocol::{
    types::ModuleId,
    wire::{
        ErrorCode, RpcError,
        events::{Event, ModuleEvent},
    },
};
use eva_rpc::{Accepted, Listener, Server, config::Listen};

use crate::{
    components::Components,
    db::{Db as _, Queries as _, stats::Metric},
    kernel::Kernel,
    shared::Shared,
    subsystems::modules::live::{Attach, Entry},
};

pub use self::auth::KernelAuth;

pub mod auth;
mod events;
pub mod live;

/// Сколько остановка ждёт, пока сессии приберут за собой.
const DRAIN: Duration = Duration::from_secs(5);

/// Поднятые слушатели; `shutdown` закрывает их и все сессии.
pub struct Listening<C: Components> {
    shared: Shared<C>,
    stop: watch::Sender<bool>,
    tasks: JoinSet<()>,
}

/// Слушатели `modules.listen`; относительный путь сокета — от `system.state_dir`.
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

    for listen in &cfg.listen {
        let listen = resolve(listen, &shared.cfg().system.state_dir);
        let listener = Listener::bind(&listen, &cfg.transport)
            .await
            .map_err(|e| eyre::eyre!("failed to listen on {}: {e}", describe(&listen)))?;
        log::info!(address = describe(&listen); "modules listener bound");

        let server = server.clone();
        let session_shared = shared.clone();
        let mut stopped = stopped.clone();
        tasks.spawn(async move {
            let shutdown = async move {
                _ = stopped.wait_for(|stop| *stop).await;
            };
            server
                .serve(listener, shutdown, move |accepted| {
                    on_session(&session_shared, accepted);
                })
                .await;
        });
    }

    Ok(Listening {
        shared,
        stop,
        tasks,
    })
}

impl<C: Components> Listening<C> {
    /// Перестаёт принимать, закрывает сессии и ждёт их уборки.
    pub async fn shutdown(mut self) {
        _ = self.stop.send(true);
        while self.tasks.join_next().await.is_some() {}

        let reason = RpcError::new(ErrorCode::Unavailable, "kernel is shutting down");
        self.shared.live().close_all(&reason);
        let drained = tokio::time::timeout(DRAIN, async {
            while !self.shared.live().is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
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
fn on_session<C: Components>(shared: &Shared<C>, accepted: Accepted) {
    let module = accepted.hello.module.clone();
    let session = accepted.welcome.session_id.clone();
    let peer = accepted.peer.clone();

    let capabilities = match shared.live().attach(&module, &session, peer.clone()) {
        Attach::Superseded => {
            peer.close(superseded());
            return;
        }
        Attach::Closing(reason, capabilities) => {
            peer.close(reason);
            capabilities
        }
        Attach::Live(capabilities) => capabilities,
    };

    if let Some(cap) = capabilities.provider.clone()
        && !accepted.welcome.models_accepted.is_empty()
    {
        let provider = accepted.module().provider(cap);
        let models = shared.set_provider(module.as_str(), Some(C::Provider::from(provider)));
        log::info!(module:% = module, models = models.iter().filter(|m| m.provider == module.as_str()).count(); "module provider registered");
    }

    if let Some(events) = &capabilities.events {
        events::forward(
            shared.clone(),
            accepted.caller.clone(),
            events.subscriptions.iter().copied().collect(),
            accepted.module(),
        );
    }

    shared.bus().publish(
        Event::ModuleConnected(ModuleEvent {
            module: module.clone(),
        }),
        None,
    );

    let shared = shared.clone();
    tokio::spawn(async move {
        let reason = peer.closed().await;
        let Some(entry) = shared.live().release(&module, &session) else {
            return;
        };
        cleanup(&shared, &module, &entry).await;
        log::info!(module:% = module, session:% = session, reason:% = reason; "module session closed");
    });
}

fn superseded() -> RpcError {
    RpcError::new(ErrorCode::Conflict, "superseded")
}

/// Вытесненная сессия: закрыть; состояние имени (строка `modules`) уже
/// принадлежит новой, поэтому прибирается только память сессии.
pub(crate) fn cleanup_superseded<C: Components>(
    shared: &Shared<C>,
    module: &ModuleId,
    mut old: Entry,
) {
    old.close(superseded());
    shared.serving().drop_module(module);
    if old.capabilities.provider.is_some() {
        shared.set_provider(module.as_str(), None);
    }
    log::info!(module:% = module, session:% = old.session; "module session superseded");
}

/// Текущая сессия имени закрылась: модуль offline (`06 §8`).
async fn cleanup<C: Components>(shared: &Shared<C>, module: &ModuleId, entry: &Entry) {
    shared.serving().drop_module(module);
    if entry.capabilities.provider.is_some() {
        shared.set_provider(module.as_str(), None);
    }

    let (now, zone) = (shared.now(), shared.day_zone());
    let name = module.clone();
    if let Err(e) = shared
        .db()
        .write(move |tx| {
            tx.modules().disconnected(&name, now)?;
            tx.stats()
                .bump(Metric::ModulesDisconnected, name.as_str(), now, zone)
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
