//! Сторона soul: handshake (`06 §2`) и диспетчер операций на каждую сессию.

use std::{pin::pin, sync::Arc, time::Duration};

use fastrace::future::FutureExt as _;
use futures::{SinkExt as _, StreamExt as _, future::BoxFuture};
use tokio::time::Instant;

use eva::data;
use eva_core::{Caller, SessionMaker};
use eva_protocol::{
    types::module::ServiceAddress,
    wire::{
        Capabilities, ErrorCode, Frame, RpcError,
        frame::{Notify, Request, Response},
        handshake::{Hello, ProtoVersion, Token, Warning, Welcome},
        method,
    },
};

use crate::{
    codec,
    config::Config,
    gate::Gate,
    module::ModuleClient,
    peer::{IdSeq, Peer},
    transport::{
        ACCEPT_BACKOFF, Conn, Listener, Outbound, TransportError, http, noise::Psk, service,
    },
};

use self::{dispatch::Table, window::Windows};

pub use self::dispatch::Dispatcher;

mod dispatch;
mod window;

/// Байт кадрирования сверх `max_hello` до `welcome`: префикс длины uds,
/// заголовок ws-кадра.
const HELLO_FRAMING: u64 = 64;

/// Проверка `hello` на стороне soul: идентичность → `welcome` (`module` и `kind` в нём
/// задают caller сессии), вытеснение прежней сессии того же модуля. Отказ
/// уходит модулю ответом на `hello`, соединение закрывается.
pub trait Authenticator: Send + Sync + 'static {
    /// Что набирающий знает о записи, которую набрал: у soul — id реестра и
    /// то, по чему попытка устаревает.
    type Dialed: Send + Sync + 'static;

    fn authenticate(
        &self,
        hello: &Hello,
        origin: Origin<'_, Self::Dialed>,
    ) -> impl Future<Output = Result<Welcome, RpcError>> + Send;

    /// Одноразовая сессия HTTP-моста (`06 §1`): токен клиента → caller по тем
    /// же правилам, что `hello.token`, но без строки `modules` и вытеснения;
    /// отказ — `unauthorized`.
    fn identify(&self, token: &Token) -> impl Future<Output = Result<Caller, RpcError>> + Send;
}

/// Кто открыл соединение (`06 §2`).
#[derive(Debug)]
pub enum Origin<'a, D> {
    /// Клиент на слушателе: идентичность — `hello.token`.
    Listener,
    /// Soul набрал запись `D`: Noise уже доказал её PSK, токена в `hello` нет.
    Dialed(&'a D),
}

impl<D> Clone for Origin<'_, D> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<D> Copy for Origin<'_, D> {}

/// Попытка `Server::dial` не стала сессией.
#[data(error, not(serde, JsonSchema))]
pub enum DialError {
    /// Сокет, DNS, Noise (в том числе чужой ключ) или таймаут до `hello`.
    #[display("{_0}")]
    Transport(TransportError),
    /// `hello` не пришёл, нарушил протокол или отвергнут; то, что ушло модулю.
    #[display("handshake: {_0}")]
    Handshake(RpcError),
}

/// Сессия модуля после `welcome`.
#[derive(Debug)]
pub struct Accepted {
    pub peer: Peer,
    pub hello: Hello,
    pub welcome: Welcome,
    /// Из `welcome.kind` и `welcome.module`; прав больше провод не несёт.
    pub caller: Caller,
}

impl Accepted {
    /// Вызовы soul → модуль в этой сессии.
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

    /// Handshake клиента на установленном соединении. `hello` обязан быть
    /// первым кадром в течение `handshake_timeout`; любой другой кадр
    /// закрывает соединение. Ошибка — то, что ушло модулю.
    pub async fn accept(&self, mut conn: Conn) -> Result<Accepted, RpcError> {
        let deadline = Instant::now() + *self.inner.cfg.handshake_timeout;
        let heard = self.hear(&mut conn, deadline).await;
        self.settle(conn, heard, Origin::Listener).await
    }

    /// Набирает сервис: connect, Noise, `hello` и `welcome` — за один
    /// `handshake_timeout` от начала connect.
    pub async fn dial(
        &self,
        address: &ServiceAddress,
        psk: &Psk,
        dialed: &A::Dialed,
    ) -> Result<Accepted, DialError> {
        let deadline = Instant::now() + *self.inner.cfg.handshake_timeout;
        let conn = service::dial_by(address, psk, &self.inner.cfg, deadline)
            .await
            .map_err(DialError::Transport)?;
        self.accept_dialed_by(conn, dialed, deadline)
            .await
            .map_err(DialError::Handshake)
    }

    /// Handshake на соединении, которое soul набрал сам (`dial`, пара в
    /// памяти): `hello` без токена и ограничен лишь `max_frame`.
    pub async fn accept_dialed(
        &self,
        conn: Conn,
        dialed: &A::Dialed,
    ) -> Result<Accepted, RpcError> {
        let deadline = Instant::now() + *self.inner.cfg.handshake_timeout;
        self.accept_dialed_by(conn, dialed, deadline).await
    }

    async fn accept_dialed_by(
        &self,
        mut conn: Conn,
        dialed: &A::Dialed,
        deadline: Instant,
    ) -> Result<Accepted, RpcError> {
        let heard = match self.read_hello(&mut conn, deadline).await {
            Ok((id, hello, _)) if hello.token.is_some() => Err((
                Some(id),
                RpcError::invalid_params("a dialed module sends no token"),
            )),
            heard => heard,
        };
        self.settle(conn, heard, Origin::Dialed(dialed)).await
    }

    /// Первый кадр под бюджетом чтения: до `welcome` пир не доказал права
    /// на кадры в `max_frame`.
    async fn hear(&self, conn: &mut Conn, deadline: Instant) -> Heard {
        let max = u64::try_from(self.inner.cfg.max_hello()).unwrap_or(u64::MAX);
        conn.meter.cap_reads(max.saturating_add(HELLO_FRAMING));
        self.read_hello(conn, deadline).await
    }

    /// `authenticate`, окно и `welcome`. Прерывать нельзя: `authenticate`
    /// заводит состояние сессии, которое прибирает только владелец `Accepted`.
    /// Поэтому сессия отдаётся и с недоставленным `welcome` — уже закрытой.
    async fn settle(
        &self,
        mut conn: Conn,
        heard: Heard,
        origin: Origin<'_, A::Dialed>,
    ) -> Result<Accepted, RpcError> {
        let cfg = &self.inner.cfg;
        let (id, hello, unknown) = match heard {
            Ok(hello) => hello,
            Err((id, error)) => return Err(reject(conn, id, error).await),
        };

        let mut welcome = match self.inner.auth.authenticate(&hello, origin).await {
            Ok(welcome) => welcome,
            Err(error) => {
                log::info!(
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

        let caller = Caller::from_kind(welcome.kind, welcome.module.clone());
        let resume = hello.resume.as_ref().map(|r| &r.session_id);
        let window = self
            .inner
            .windows
            .open(&welcome.session_id, &welcome.module, resume);
        welcome.resumed = window.resumed();

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
                module:% = welcome.module,
                session:% = welcome.session_id,
                kind:% = welcome.kind,
                remote = remote.as_str(),
                resume = hello.resume.is_some(),
                resumed = welcome.resumed;
                "module session opened"
            ),
            Err(error) => {
                log::info!(
                    module:% = welcome.module,
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
    async fn read_hello(&self, conn: &mut Conn, deadline: Instant) -> Heard {
        let cfg = &self.inner.cfg;

        let raw = match tokio::time::timeout_at(deadline, conn.rx.next()).await {
            Err(_) => {
                let error = RpcError::new(ErrorCode::Timeout, "no hello within handshake_timeout");
                return Err((None, error));
            }
            Ok(None) => return Err((None, codec::unavailable("closed before hello"))),
            // Отказ по существу, а не обрыв: иначе модуль с крупным каталогом
            // тулов переподключался бы вечно.
            Ok(Some(Err(_))) if conn.meter.exhausted() => {
                let error = RpcError::invalid_params(format_args!(
                    "hello exceeds max_hello of {} bytes",
                    cfg.max_hello()
                ));
                return Err((None, error));
            }
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
            let message = format!(
                "protocol {} is not supported: major {}",
                hello.proto_version,
                ProtoVersion::CURRENT.major
            );
            return Err((Some(id), RpcError::new(ErrorCode::ProtoVersion, message)));
        }

        Ok((id, hello, unknown_capabilities(&request.params)))
    }

    /// Цикл приёма: TLS/Upgrade и handshake — в своей задаче на соединение,
    /// принятые сессии — в `on_session`. TLS, Upgrade и `hello` укладываются
    /// в один `handshake_timeout`; соединения до `hello` держит `Gate`.
    /// Слушатель HTTP-моста сессий не даёт: его запросы идут в диспетчер напрямую.
    pub async fn serve<F>(
        &self,
        listener: Listener,
        shutdown: impl Future<Output = ()>,
        on_session: F,
    ) where
        F: Fn(Accepted) + Send + Sync + 'static,
    {
        if let Some(bound) = listener.http() {
            let exchange: Arc<dyn http::Exchange> = Arc::new(self.clone());
            let inner = &self.inner;
            return http::serve(bound, &inner.cfg, &inner.gate, shutdown, exchange).await;
        }

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
                let hear = async {
                    let mut conn = match tokio::time::timeout_at(deadline, raw.upgrade()).await {
                        Ok(Ok(conn)) => conn,
                        Ok(Err(e)) => {
                            log::debug!(error:% = e; "transport handshake failed");
                            return None;
                        }
                        Err(_) => {
                            log::debug!("transport handshake timed out");
                            return None;
                        }
                    };
                    let heard = server.hear(&mut conn, deadline).await;
                    Some((conn, heard))
                };
                let (conn, heard) = tokio::select! {
                    () = pass.evicted() => {
                        log::debug!(ip:? = pass.ip(); "silent connection evicted before hello");
                        return;
                    }
                    heard = hear => match heard {
                        Some(heard) => heard,
                        None => return,
                    },
                };
                pass.spoke();
                let accepted = server.settle(conn, heard, Origin::Listener).await;
                drop(pass);
                if let Ok(accepted) = accepted {
                    on_session(accepted);
                }
            });
        }
    }
}

impl<M, A> http::Exchange for Server<M, A>
where
    M: SessionMaker + 'static,
    M::Session: 'static,
    A: Authenticator,
{
    fn identify<'a>(&'a self, token: &'a Token) -> BoxFuture<'a, Result<Caller, RpcError>> {
        Box::pin(self.inner.auth.identify(token))
    }

    fn dispatch<'a>(
        &'a self,
        caller: Caller,
        method: &'a str,
        call: http::Call,
    ) -> BoxFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let run = match self.inner.table.http(method) {
                Ok(run) => run,
                Err(error) => return call.failed(&error).body,
            };
            let reply = match self.inner.maker.make_session(caller.clone()).await {
                Ok(session) => {
                    run(session, call)
                        .in_span(dispatch::root_span(method, &caller))
                        .await
                }
                Err(aux) => call.typed(&aux),
            };
            if let Some((code, error)) = &reply.failure {
                dispatch::log_failure(method, caller.label(), *code, error);
            }
            reply.body
        })
    }
}

/// Ответ на `hello` ошибкой и закрытие; без `id` причина уходит
/// уведомлением `close`, ведь у uds нет close-кадра транспорта.
async fn reject(mut conn: Conn, id: Option<u64>, error: RpcError) -> RpcError {
    let close = async {
        let frame = match id {
            Some(id) => Some(Frame::Response(Response::err(id, error.clone()))),
            None => codec::encode(&error).ok().map(|params| {
                Frame::Notify(Notify {
                    method: method::CLOSE.to_owned(),
                    params,
                })
            }),
        };
        if let Some(raw) = frame.and_then(|frame| codec::encode_frame(&frame).ok()) {
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

/// `hello` либо `id` для ответа (если запрос был) и причина закрытия.
type Heard = Result<HelloRequest, (Option<u64>, RpcError)>;

/// Ключи capabilities как пришли: serde молча пропускает незнакомые поля,
/// а модулю нужно знать, что soul их не понял (`06 §8`).
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
