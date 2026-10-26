//! Сервис (`06 §1`, `§10`): модуль слушает, soul набирает его по записи
//! реестра. Каждое соединение от soul — Noise responder и `hello` без токена;
//! соединение после `welcome` становится текущим, прежнее закрывается
//! `conflict "superseded"`.

use std::{
    fmt,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::task::{AbortHandle, JoinHandle, JoinSet};

use eva_protocol::{
    types::{ModuleId, module::ServiceAddress},
    wire::{ErrorCode, Welcome},
};
use eva_rpc::{
    ConnectError, Incoming, Inherited, Psk, RpcClient, ServiceListen, ServiceListener,
    TransportError,
    gate::{Gate, Pass},
};

use crate::{Soul, module::Runtime};

/// Пауза после отказа `accept` (например, `EMFILE`): иначе цикл крутился бы вхолостую.
const ACCEPT_PAUSE: Duration = Duration::from_millis(100);

/// Почему не удалось последнее соединение от soul.
type Failure = Arc<Mutex<Option<String>>>;

/// Сервис, который ждёт звонков от soul; клоны делят слушателя — `run` съедает один из них.
#[derive(Clone)]
pub struct Listening {
    runtime: Arc<Runtime>,
    accept: Arc<Accept>,
}

/// Фоновый приём соединений от soul; останавливается с последним клоном `Listening`.
struct Accept {
    abort: AbortHandle,
    task: Mutex<Option<JoinHandle<()>>>,
    address: Option<ServiceAddress>,
    soul_wait: Duration,
    failure: Failure,
}

impl Drop for Accept {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

impl Listening {
    pub(crate) fn start(
        runtime: Arc<Runtime>,
        listener: ServiceListener,
        handshake: Duration,
        soul_wait: Duration,
    ) -> Self {
        let address = listener.local_addr();
        let shown = address
            .as_ref()
            .map_or_else(|| "unknown".to_owned(), ToString::to_string);
        log::info!(address = shown.as_str(), about = runtime.about(); "listening for the soul");

        let failure = Failure::default();
        let task = tokio::spawn(accept(
            runtime.clone(),
            listener,
            handshake,
            failure.clone(),
        ));
        Self {
            runtime,
            accept: Arc::new(Accept {
                abort: task.abort_handle(),
                task: Mutex::new(Some(task)),
                address,
                soul_wait,
                failure,
            }),
        }
    }

    /// Адрес, по которому звонит soul (`modules.services.<id>.address`); порт
    /// `0` из конфига здесь уже выбран системой.
    pub fn address(&self) -> Option<ServiceAddress> {
        self.accept.address.clone()
    }

    /// Soul через текущее соединение: ручка следует за новыми соединениями
    /// soul (`live`). До первого ждёт его до `module.soul_wait`; не
    /// дождался — ошибка с причиной последней неудачной попытки soul.
    pub async fn soul(&self) -> Result<Soul, ConnectError> {
        let wait = self.accept.soul_wait;
        match tokio::time::timeout(wait, self.runtime.link.first()).await {
            Ok(Some(soul)) => Ok(soul),
            Ok(None) => Err(ConnectError::Transport(
                "the module stopped listening before the soul connected".to_owned(),
            )),
            Err(_) => {
                let last = lock(&self.accept.failure).clone();
                let message = match last {
                    Some(e) => {
                        format!("the soul did not connect within {wait:?}; last attempt: {e}")
                    }
                    None => format!("the soul did not connect within {wait:?}"),
                };
                Err(ConnectError::Transport(message))
            }
        }
    }

    /// Id модуля из `welcome`: ключ записи реестра у soul (`06 §2`); до
    /// первого соединения — `None`.
    pub fn module(&self) -> Option<ModuleId> {
        self.runtime.module()
    }

    /// Последний `welcome`: `warnings`, `models_accepted`, `resumed`…
    pub fn welcome(&self) -> Option<Welcome> {
        self.runtime.current(|c| c.welcome.clone())
    }

    /// Текущий `RpcClient` — сырой пир.
    pub fn client(&self) -> Option<RpcClient> {
        self.runtime.current(|c| c.client.clone())
    }

    /// Живёт до `shutdown`: ни потеря soul, ни вытеснение не завершают его — soul
    /// перезвонит сам. На `shutdown` приём останавливается, текущее
    /// соединение закрывается `unavailable`.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> eyre::Result<()> {
        let task = lock(&self.accept.task).take();
        let result = match task {
            Some(mut task) => tokio::select! {
                () = shutdown => {
                    task.abort();
                    _ = task.await;
                    Ok(())
                }
                stopped = &mut task => Err(match stopped {
                    Ok(()) => eyre::eyre!("the soul listener stopped"),
                    Err(e) => eyre::eyre!("the soul listener failed: {e}"),
                }),
            },
            None => {
                shutdown.await;
                Ok(())
            }
        };
        self.runtime.link.close();
        result
    }
}

impl fmt::Debug for Listening {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Listening")
            .field("address", &self.accept.address)
            .field("module", &self.runtime.module())
            .field(
                "session",
                &self.runtime.current(|c| c.welcome.session_id.clone()),
            )
            .finish_non_exhaustive()
    }
}

async fn accept(
    runtime: Arc<Runtime>,
    listener: ServiceListener,
    handshake: Duration,
    failure: Failure,
) {
    // Пока идёт приём, soul перезвонит: вызов на закрытом соединении стоит ждать.
    let _reconnecting = runtime.link.reconnecting();
    let gate = Gate::new();
    let mut conns = JoinSet::new();
    loop {
        tokio::select! {
            incoming = listener.accept() => match incoming {
                Ok(incoming) => match gate.admit(incoming.ip()) {
                    Some(pass) => {
                        let attempt = serve(runtime.clone(), incoming, pass, handshake, failure.clone());
                        conns.spawn(attempt);
                    }
                    None => {
                        log::debug!(remote = incoming.remote(); "too many connections before noise, dropped");
                    }
                },
                Err(e) => {
                    log::warn!(error:% = e; "accepting a soul connection failed");
                    tokio::time::sleep(ACCEPT_PAUSE).await;
                }
            },
            Some(joined) = conns.join_next() => {
                if let Err(e) = joined {
                    log::error!(error:% = e; "soul connection task failed");
                }
            }
        }
    }
}

/// Одно соединение от soul: Noise, `hello`, `welcome` — за один `handshake`;
/// затем ждёт закрытия ради лога.
async fn serve(
    runtime: Arc<Runtime>,
    incoming: Incoming,
    pass: Pass,
    handshake: Duration,
    failure: Failure,
) {
    let remote = incoming.remote().to_owned();
    let proven = AtomicBool::new(false);
    let attempt = async {
        let conn = incoming.secure().await.map_err(|e| e.to_string())?;
        // PSK доказан — это soul: до `welcome` его место не отдают.
        proven.store(true, Ordering::Release);
        pass.spoke();
        runtime.answer(conn).await.map_err(|e| e.to_string())
    };
    let outcome = tokio::select! {
        () = pass.evicted() => Err("evicted by newer connections before noise".to_owned()),
        outcome = tokio::time::timeout(handshake, attempt) => {
            outcome.unwrap_or_else(|_| Err("handshake timed out".to_owned()))
        }
    };
    drop(pass);
    let client = match outcome {
        Ok(client) => client,
        Err(e) => {
            // До Noise пир не опознан: сканер не должен топить лог.
            if proven.load(Ordering::Acquire) {
                log::warn!(remote = remote.as_str(), error = e.as_str(); "soul connection failed");
            } else {
                log::debug!(remote = remote.as_str(), error = e.as_str(); "connection failed before noise");
            }
            *lock(&failure) = Some(e);
            return;
        }
    };

    let reason = client.closed().await;
    // `conflict` — смена соединения (своя или со стороны soul), остановка модуля — не потеря.
    if reason.code != ErrorCode::Conflict && !runtime.link.is_closed() {
        log::warn!(module:? = runtime.module(), reason:% = reason; "soul connection lost, waiting for the soul to dial again");
    }
}

/// Слушатель сервиса: сокет менеджера служб с адресом `listen` или свой bind.
pub(crate) async fn bind(
    listen: &ServiceListen,
    psk: Psk,
    cfg: &eva_rpc::Config,
) -> Result<ServiceListener, ConnectError> {
    let config = |e: TransportError| ConnectError::Config(format!("module.listen: {e}"));
    let mut inherited = Inherited::take().map_err(config)?;
    let listener = ServiceListener::open(listen, &mut inherited, psk, cfg)
        .await
        .map_err(|e| match e {
            TransportError::Activation(_) => config(e),
            e => ConnectError::Transport(format!("module.listen: {e}")),
        })?;
    inherited.finish().map_err(config)?;
    Ok(listener)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
