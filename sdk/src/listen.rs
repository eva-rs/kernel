//! Сервис (`06 §1`, `§10`): модуль слушает, ядро набирает его по записи
//! реестра. Каждое соединение ядра — Noise responder и `hello` без токена;
//! соединение после `welcome` становится текущим, прежнее закрывается
//! `conflict "superseded"`.

use std::{
    fmt,
    os::fd::{FromRawFd as _, OwnedFd, RawFd},
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
    ConnectError, Incoming, Psk, RpcClient, ServiceListen, ServiceListener, TransportError,
};

use crate::{Kernel, module::Runtime};

/// Пауза после отказа `accept` (например, `EMFILE`): иначе цикл крутился бы вхолостую.
const ACCEPT_PAUSE: Duration = Duration::from_millis(100);

/// Первый унаследованный сокет по протоколу `sd_listen_fds`.
const LISTEN_FDS_START: RawFd = 3;

/// Почему не удалось последнее соединение ядра.
type Failure = Arc<Mutex<Option<String>>>;

/// Сервис, который слушает ядро; клоны делят слушателя — `run` съедает один из них.
#[derive(Clone)]
pub struct Listening {
    runtime: Arc<Runtime>,
    accept: Arc<Accept>,
}

/// Фоновый приём соединений ядра; останавливается с последним клоном `Listening`.
struct Accept {
    abort: AbortHandle,
    task: Mutex<Option<JoinHandle<()>>>,
    address: Option<ServiceAddress>,
    kernel_wait: Duration,
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
        kernel_wait: Duration,
    ) -> Self {
        let address = listener.local_addr();
        let shown = address
            .as_ref()
            .map_or_else(|| "unknown".to_owned(), ToString::to_string);
        log::info!(address = shown.as_str(), about = runtime.about(); "listening for the kernel");

        let failure = Failure::default();
        let task = tokio::spawn(accept(runtime.clone(), listener, failure.clone()));
        Self {
            runtime,
            accept: Arc::new(Accept {
                abort: task.abort_handle(),
                task: Mutex::new(Some(task)),
                address,
                kernel_wait,
                failure,
            }),
        }
    }

    /// Адрес, который набирает ядро (`modules.services.<id>.address`); порт
    /// `0` из конфига здесь уже выбран системой.
    pub fn address(&self) -> Option<ServiceAddress> {
        self.accept.address.clone()
    }

    /// Ядро через текущее соединение: ручка следует за новыми соединениями
    /// ядра (`live`). До первого ждёт его до `module.kernel_wait`; не
    /// дождался — ошибка с причиной последней неудачной попытки ядра.
    pub async fn kernel(&self) -> Result<Kernel, ConnectError> {
        let wait = self.accept.kernel_wait;
        match tokio::time::timeout(wait, self.runtime.link.first()).await {
            Ok(Some(kernel)) => Ok(kernel),
            Ok(None) => Err(ConnectError::Transport(
                "the module stopped listening before the kernel connected".to_owned(),
            )),
            Err(_) => {
                let last = lock(&self.accept.failure).clone();
                let message = match last {
                    Some(e) => {
                        format!("the kernel did not connect within {wait:?}; last attempt: {e}")
                    }
                    None => format!("the kernel did not connect within {wait:?}"),
                };
                Err(ConnectError::Transport(message))
            }
        }
    }

    /// Id модуля из `welcome`: ключ записи реестра у ядра (`06 §2`); до
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

    /// Живёт до `shutdown`: потеря ядра и вытеснение его не завершают — ядро
    /// перезвонит само. На `shutdown` приём останавливается, текущее
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
                    Ok(()) => eyre::eyre!("the kernel listener stopped"),
                    Err(e) => eyre::eyre!("the kernel listener failed: {e}"),
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

async fn accept(runtime: Arc<Runtime>, listener: ServiceListener, failure: Failure) {
    // Пока идёт приём, ядро перезвонит: вызов на закрытом соединении стоит ждать.
    let _reconnecting = runtime.link.reconnecting();
    let mut conns = JoinSet::new();
    loop {
        tokio::select! {
            incoming = listener.accept() => match incoming {
                Ok(incoming) => {
                    conns.spawn(serve(runtime.clone(), incoming, failure.clone()));
                }
                Err(e) => {
                    log::warn!(error:% = e; "accepting a kernel connection failed");
                    tokio::time::sleep(ACCEPT_PAUSE).await;
                }
            },
            Some(joined) = conns.join_next() => {
                if let Err(e) = joined {
                    log::error!(error:% = e; "kernel connection task failed");
                }
            }
        }
    }
}

/// Одно соединение ядра: Noise, `hello`, `welcome`; затем ждёт закрытия ради лога.
async fn serve(runtime: Arc<Runtime>, incoming: Incoming, failure: Failure) {
    let remote = incoming.remote().to_owned();
    let attempt = async {
        let conn = incoming.secure().await.map_err(|e| e.to_string())?;
        runtime.answer(conn).await.map_err(|e| e.to_string())
    };
    let client = match attempt.await {
        Ok(client) => client,
        Err(e) => {
            log::warn!(remote = remote.as_str(), error = e.as_str(); "kernel connection failed");
            *lock(&failure) = Some(e);
            return;
        }
    };

    let reason = client.closed().await;
    // `conflict` — смена соединения (своя или ядра), остановка модуля — не потеря.
    if reason.code != ErrorCode::Conflict && !runtime.link.is_closed() {
        log::warn!(module:? = runtime.module(), reason:% = reason; "kernel connection lost, waiting for the kernel to dial again");
    }
}

/// Слушатель сервиса: сокет от менеджера служб или свой bind.
pub(crate) async fn bind(
    listen: &ServiceListen,
    psk: Psk,
    cfg: &eva_rpc::Config,
) -> Result<ServiceListener, ConnectError> {
    let transport = |e: TransportError| ConnectError::Transport(format!("module.listen: {e}"));
    let Some(fd) = inherited()? else {
        return ServiceListener::bind(listen, psk, cfg)
            .await
            .map_err(transport);
    };
    let listener = ServiceListener::adopt(listen, fd, psk, cfg).map_err(transport)?;
    // Сокет другого вида (или не сокет) адреса не даёт.
    if listener.local_addr().is_none() {
        let kind = match listen {
            ServiceListen::Uds(_) => "uds",
            ServiceListen::Tcp(_) => "tcp",
        };
        return Err(ConnectError::Config(format!(
            "module.listen: the inherited socket is not a {kind} listener"
        )));
    }
    Ok(listener)
}

/// fd 3 от менеджера служб, если `LISTEN_PID` — этот процесс; переменные
/// снимаются, чтобы их не унаследовали дети.
fn inherited() -> Result<Option<OwnedFd>, ConnectError> {
    static TAKEN: AtomicBool = AtomicBool::new(false);

    let pid = std::env::var("LISTEN_PID").ok();
    let fds = std::env::var("LISTEN_FDS").ok();
    if !activated(pid.as_deref(), fds.as_deref(), std::process::id())
        .map_err(ConnectError::Config)?
    {
        return Ok(None);
    }
    if TAKEN.swap(true, Ordering::AcqRel) {
        return Err(ConnectError::Config(
            "module.listen: the inherited socket is already taken".to_owned(),
        ));
    }
    for var in ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"] {
        // SAFETY: как `sd_listen_fds(1)` на старте модуля: чтения окружения из std
        // сериализованы, одновременного `getenv` из C-кода в этот момент нет.
        unsafe { std::env::remove_var(var) };
    }
    // SAFETY: менеджер служб передал fd 3 этому процессу (`LISTEN_PID`), `TAKEN` не даёт взять его дважды.
    let inherited = unsafe { OwnedFd::from_raw_fd(LISTEN_FDS_START) };
    // Копия получает CLOEXEC: слушатель не утечёт в дочерние процессы модуля.
    inherited
        .try_clone()
        .map(Some)
        .map_err(|e| ConnectError::Transport(format!("module.listen: inherited socket: {e}")))
}

/// Решение `sd_listen_fds`: сокеты наши, только если `LISTEN_PID` — этот
/// процесс; модуль слушает ровно один.
fn activated(pid: Option<&str>, fds: Option<&str>, me: u32) -> Result<bool, String> {
    if pid.and_then(|pid| pid.trim().parse::<u32>().ok()) != Some(me) {
        return Ok(false);
    }
    match fds.map(|n| n.trim().parse::<u32>()) {
        None | Some(Ok(0)) => Ok(false),
        Some(Ok(1)) => Ok(true),
        Some(Ok(n)) => Err(format!(
            "socket activation passes {n} sockets; the module listens on exactly one"
        )),
        Some(Err(_)) => Err(format!("LISTEN_FDS is not a number: {fds:?}")),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::activated;

    #[test]
    fn sockets_belong_to_the_named_process_only() {
        assert_eq!(activated(None, None, 7), Ok(false));
        assert_eq!(activated(Some("8"), Some("1"), 7), Ok(false));
        assert_eq!(activated(Some("junk"), Some("1"), 7), Ok(false));
        assert_eq!(activated(Some("7"), None, 7), Ok(false));
        assert_eq!(activated(Some("7"), Some("0"), 7), Ok(false));
        assert_eq!(activated(Some("7"), Some("1"), 7), Ok(true));
        assert!(activated(Some("7"), Some("2"), 7).is_err());
        assert!(activated(Some("7"), Some("x"), 7).is_err());
    }
}
