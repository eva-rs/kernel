//! Сторона ядра: handshake (`06 §2`) и диспетчер операций на каждую сессию.

use std::{
    collections::HashMap,
    net::IpAddr,
    pin::pin,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use futures::{SinkExt as _, StreamExt as _};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::Instant,
};

use eva_core::{Caller, SessionMaker};
use eva_protocol::wire::{
    Capabilities, ErrorCode, Frame, RpcError,
    frame::{Request, Response},
    handshake::{Hello, ProtoVersion, Warning, Welcome},
    method,
};

use crate::{
    codec,
    config::Config,
    module::ModuleClient,
    peer::{IdSeq, Peer},
    transport::{Conn, Listener, Outbound},
};

use self::{dispatch::Table, window::Windows};

pub use self::dispatch::Dispatcher;

mod dispatch;
mod window;

/// Пауза после отказа `accept`: исчерпанные дескрипторы не крутят цикл вхолостую.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Соединений до `welcome` на сервер.
const MAX_HANDSHAKES: usize = 64;

/// Соединений до `welcome` с одного IP.
const MAX_HANDSHAKES_PER_IP: usize = 8;

/// Байт, которые пир может прислать до `welcome`: `hello` с capabilities
/// в него укладывается, а кадр в `max_frame` без токена — нет.
const PREAUTH_READ: usize = 1 << 20;

/// Проверка `hello` ядром: токен → `welcome` (бит `kind` в нём задаёт
/// caller сессии), зарезервированные имена, вытеснение прежней сессии того
/// же модуля. Отказ уходит модулю ответом на `hello`, соединение закрывается.
pub trait Authenticator: Send + Sync + 'static {
    fn authenticate(&self, hello: &Hello)
    -> impl Future<Output = Result<Welcome, RpcError>> + Send;
}

/// Сессия модуля после `welcome`.
#[derive(Debug)]
pub struct Accepted {
    pub peer: Peer,
    pub hello: Hello,
    pub welcome: Welcome,
    /// Из `welcome.kind` и `hello.module`; прав больше провод не несёт.
    pub caller: Caller,
}

impl Accepted {
    /// Вызовы ядро → модуль в этой сессии.
    pub fn module(&self) -> ModuleClient {
        ModuleClient::new(self.peer.clone())
    }
}

/// Сервер RPC: `hello` → `Authenticator` → диспетчер над `SessionMaker`.
pub struct Server<M: SessionMaker, A> {
    inner: Arc<ServerInner<M, A>>,
}

struct ServerInner<M: SessionMaker, A> {
    maker: Arc<M>,
    auth: A,
    cfg: Config,
    table: Arc<Table<M::Session>>,
    windows: Windows,
    gate: Arc<Gate>,
}

impl<M: SessionMaker, A> Clone for Server<M, A> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<M, A> Server<M, A>
where
    M: SessionMaker + 'static,
    M::Session: 'static,
    A: Authenticator,
{
    pub fn new(maker: M, auth: A, cfg: Config) -> Self {
        let windows = Windows::new(*cfg.idempotency_window);
        Self {
            inner: Arc::new(ServerInner {
                maker: Arc::new(maker),
                auth,
                cfg,
                table: Arc::new(Table::new()),
                windows,
                gate: Gate::new(),
            }),
        }
    }

    pub fn config(&self) -> &Config {
        &self.inner.cfg
    }

    /// Handshake на установленном соединении. `hello` обязан быть первым
    /// кадром в течение `handshake_timeout`; любой другой кадр закрывает
    /// соединение. Ошибка — то, что ушло модулю.
    pub async fn accept(&self, conn: Conn) -> Result<Accepted, RpcError> {
        let deadline = Instant::now() + *self.inner.cfg.handshake_timeout;
        self.accept_by(conn, deadline).await
    }

    /// `hello` — до `deadline`. Если `authenticate` прошёл, сессия отдаётся
    /// даже с недоставленным `welcome` — уже закрытой: заведённое им
    /// состояние прибирает владелец по закрытию, как у любой сессии.
    async fn accept_by(&self, mut conn: Conn, deadline: Instant) -> Result<Accepted, RpcError> {
        let cfg = &self.inner.cfg;
        // До `welcome` пир не доказал права на кадры в `max_frame`.
        conn.meter
            .cap_reads(u64::try_from(PREAUTH_READ).unwrap_or(u64::MAX));

        let (id, hello, unknown) = match self.read_hello(&mut conn, deadline).await {
            Ok(hello) => hello,
            Err((id, error)) => return Err(reject(conn, id, error).await),
        };

        let mut welcome = match self.inner.auth.authenticate(&hello).await {
            Ok(welcome) => welcome,
            Err(error) => {
                log::info!(
                    module = codec::shown(hello.module.as_str()).as_str(),
                    remote = conn.remote.as_str(),
                    code:% = error.code,
                    error = codec::shown(&error.message).as_str();
                    "module rejected"
                );
                return Err(reject(conn, Some(id), error).await);
            }
        };
        conn.meter.lift_cap();
        welcome.proto_version = ProtoVersion::CURRENT;
        welcome
            .warnings
            .extend(unknown.into_iter().map(|key| Warning {
                code: ErrorCode::InvalidParams,
                subject: format!("capabilities.{key}"),
                message: String::from("unknown capability, ignored"),
            }));

        let caller = Caller::from_token(welcome.kind, hello.module.clone());
        let resume = hello.resume.as_ref().map(|r| &r.session_id);
        let window = self
            .inner
            .windows
            .open(&welcome.session_id, &hello.module, resume);

        let reply = codec::encode(&welcome)
            .map_err(|e| RpcError::new(ErrorCode::Internal, e))
            .and_then(|welcome| {
                codec::encode_frame(&Frame::Response(Response::ok(id, welcome)))
                    .map_err(|e| RpcError::new(ErrorCode::Internal, e))
            });
        let delivered = match reply {
            Ok(reply) => conn
                .tx
                .send(Outbound::Frame(reply))
                .await
                .map_err(|e| codec::unavailable(format_args!("welcome not delivered: {e}"))),
            Err(error) => Err(error),
        };

        let dispatcher = Dispatcher::new(
            self.inner.maker.clone(),
            caller.clone(),
            self.inner.table.clone(),
            window,
        );
        let remote = conn.remote.clone();
        let peer = Peer::spawn(conn, cfg.clone(), IdSeq::server(), dispatcher);

        match delivered {
            Ok(()) => log::info!(
                module:% = hello.module,
                session:% = welcome.session_id,
                kind:% = welcome.kind,
                remote = remote.as_str(),
                resumed = hello.resume.is_some();
                "module session opened"
            ),
            Err(error) => {
                log::info!(
                    module:% = hello.module,
                    session:% = welcome.session_id,
                    remote = remote.as_str(),
                    error:% = error;
                    "module session lost before welcome"
                );
                peer.close(error);
            }
        }

        Ok(Accepted {
            peer,
            hello,
            welcome,
            caller,
        })
    }

    /// Первый кадр — `request hello` поддерживаемой версии; иначе `id` для
    /// ответа (если запрос был) и причина закрытия.
    async fn read_hello(
        &self,
        conn: &mut Conn,
        deadline: Instant,
    ) -> Result<HelloRequest, (Option<u64>, RpcError)> {
        let cfg = &self.inner.cfg;

        let raw = match tokio::time::timeout_at(deadline, conn.rx.next()).await {
            Err(_) => {
                let error = RpcError::new(ErrorCode::Timeout, "no hello within handshake_timeout");
                return Err((None, error));
            }
            Ok(None) => return Err((None, codec::unavailable("closed before hello"))),
            Ok(Some(Err(e))) => return Err((None, codec::unavailable(e))),
            Ok(Some(Ok(raw))) => raw,
        };

        let request = match codec::decode_frame(&raw) {
            Ok(Frame::Request(request)) if request.method == method::HELLO => request,
            Ok(Frame::Request(Request { id, .. })) => {
                return Err((
                    Some(id),
                    RpcError::invalid_params("the first request must be hello"),
                ));
            }
            Ok(_) => {
                return Err((
                    None,
                    RpcError::invalid_params("the first frame must be request hello"),
                ));
            }
            Err(e) => {
                return Err((
                    None,
                    RpcError::invalid_params(format!("malformed frame: {e}")),
                ));
            }
        };
        let id = request.id;

        let hello: Hello =
            codec::decode(&request.params).map_err(|e| (Some(id), codec::bad_params(e)))?;
        if !cfg.accepts(hello.proto_version) {
            let current = ProtoVersion::CURRENT.major;
            let min = cfg.proto_min_major.unwrap_or(current);
            let message = format!(
                "protocol {} is not supported: major {min}..={current}",
                hello.proto_version
            );
            return Err((Some(id), RpcError::new(ErrorCode::ProtoVersion, message)));
        }

        Ok((id, hello, unknown_capabilities(&request.params)))
    }

    /// Цикл приёма: TLS/Upgrade и handshake — в своей задаче на соединение,
    /// принятые сессии — в `on_session`. TLS, Upgrade и `hello` укладываются
    /// в один `handshake_timeout`; соединений до `welcome` не больше
    /// `MAX_HANDSHAKES` (и `MAX_HANDSHAKES_PER_IP` с одного адреса), лишние
    /// закрываются сразу — иначе молчащие сокеты без токена выбрали бы дескрипторы.
    pub async fn serve<F>(
        &self,
        listener: Listener,
        shutdown: impl Future<Output = ()>,
        on_session: F,
    ) where
        F: Fn(Accepted) + Send + Sync + 'static,
    {
        let on_session = Arc::new(on_session);
        let mut shutdown = pin!(shutdown);

        loop {
            let raw = tokio::select! {
                () = &mut shutdown => break,
                raw = listener.accept() => raw,
            };
            let raw = match raw {
                Ok(raw) => raw,
                Err(e) => {
                    log::warn!(error:% = e; "accept failed");
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                    continue;
                }
            };
            let Some(pass) = self.inner.gate.admit(raw.ip()) else {
                log::debug!(ip:? = raw.ip(); "too many connections before welcome, dropped");
                continue;
            };

            let server = self.clone();
            let on_session = on_session.clone();
            tokio::spawn(async move {
                let deadline = Instant::now() + *server.inner.cfg.handshake_timeout;
                let conn = match tokio::time::timeout_at(deadline, raw.upgrade()).await {
                    Ok(Ok(conn)) => conn,
                    Ok(Err(e)) => {
                        log::debug!(error:% = e; "transport handshake failed");
                        return;
                    }
                    Err(_) => {
                        log::debug!("transport handshake timed out");
                        return;
                    }
                };
                let accepted = server.accept_by(conn, deadline).await;
                drop(pass);
                if let Ok(accepted) = accepted {
                    on_session(accepted);
                }
            });
        }
    }
}

/// Места для соединений до `welcome`: всего и на один IP.
struct Gate {
    total: Arc<Semaphore>,
    per_ip: Mutex<HashMap<IpAddr, usize>>,
}

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            total: Arc::new(Semaphore::new(MAX_HANDSHAKES)),
            per_ip: Mutex::default(),
        })
    }

    fn per_ip(&self) -> MutexGuard<'_, HashMap<IpAddr, usize>> {
        self.per_ip.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn admit(self: &Arc<Self>, ip: Option<IpAddr>) -> Option<Pass> {
        let permit = self.total.clone().try_acquire_owned().ok()?;
        if let Some(ip) = ip {
            let mut per_ip = self.per_ip();
            let count = per_ip.entry(ip).or_default();
            if *count >= MAX_HANDSHAKES_PER_IP {
                return None;
            }
            *count += 1;
        }
        Some(Pass {
            gate: self.clone(),
            ip,
            _permit: permit,
        })
    }
}

/// Место соединения в `Gate` до конца handshake.
struct Pass {
    gate: Arc<Gate>,
    ip: Option<IpAddr>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for Pass {
    fn drop(&mut self) {
        let Some(ip) = self.ip else {
            return;
        };
        let mut per_ip = self.gate.per_ip();
        if let Some(count) = per_ip.get_mut(&ip) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                per_ip.remove(&ip);
            }
        }
    }
}

/// Ответ на `hello` ошибкой (если есть `id`) и закрытие.
async fn reject(mut conn: Conn, id: Option<u64>, error: RpcError) -> RpcError {
    let close = async {
        if let Some(id) = id
            && let Ok(raw) = codec::encode_frame(&Frame::Response(Response::err(id, error.clone())))
        {
            _ = conn.tx.send(Outbound::Frame(raw)).await;
        }
        _ = conn.tx.send(Outbound::Close(Some(error.clone()))).await;
        _ = conn.tx.close().await;
    };
    _ = tokio::time::timeout(Duration::from_secs(5), close).await;
    error
}

/// Разобранный `hello`: id запроса, сам `hello` и незнакомые ключи capabilities.
type HelloRequest = (u64, Hello, Vec<String>);

/// Ключи capabilities как пришли: serde молча пропускает незнакомые поля,
/// а модулю нужно знать, что ядро их не поняло (`06 §8`).
fn unknown_capabilities(params: &[u8]) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(default)]
        capabilities: std::collections::BTreeMap<String, serde::de::IgnoredAny>,
    }

    codec::decode::<Raw>(params)
        .map(|raw| {
            raw.capabilities
                .into_keys()
                .filter(|key| !Capabilities::KEYS.contains(&key.as_str()))
                .collect()
        })
        .unwrap_or_default()
}
