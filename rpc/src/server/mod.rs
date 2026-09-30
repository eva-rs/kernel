//! Сторона ядра: handshake (`06 §2`) и диспетчер операций на каждую сессию.

use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv6Addr},
    pin::pin,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use futures::{SinkExt as _, StreamExt as _};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use eva::data;
use eva_core::{Caller, SessionMaker};
use eva_protocol::{
    types::module::ServiceAddress,
    wire::{
        Capabilities, ErrorCode, Frame, RpcError,
        frame::{Notify, Request, Response},
        handshake::{Hello, ProtoVersion, Warning, Welcome},
        method,
    },
};

use crate::{
    codec,
    config::Config,
    module::ModuleClient,
    peer::{IdSeq, Peer},
    transport::{Conn, Listener, Outbound, TransportError, noise::Psk, service},
};

use self::{dispatch::Table, window::Windows};

pub use self::dispatch::Dispatcher;

mod dispatch;
mod window;

/// Пауза после отказа `accept`: исчерпанные дескрипторы не крутят цикл вхолостую.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Соединений до `welcome` на сервер.
const MAX_HANDSHAKES: usize = 64;

/// Соединений до `welcome` из одной сети пира (`Net`).
const MAX_HANDSHAKES_PER_NET: usize = 8;

/// Байт кадрирования сверх `max_hello` до `welcome`: префикс длины uds,
/// заголовок ws-кадра.
const HELLO_FRAMING: u64 = 64;

/// Проверка `hello` ядром: идентичность → `welcome` (`module` и `kind` в нём
/// задают caller сессии), вытеснение прежней сессии того же модуля. Отказ
/// уходит модулю ответом на `hello`, соединение закрывается.
pub trait Authenticator: Send + Sync + 'static {
    /// Что набирающий знает о записи, которую набрал: у ядра — id реестра и
    /// то, по чему попытка устаревает.
    type Dialed: Send + Sync + 'static;

    fn authenticate(
        &self,
        hello: &Hello,
        origin: Origin<'_, Self::Dialed>,
    ) -> impl Future<Output = Result<Welcome, RpcError>> + Send;
}

/// Кто открыл соединение (`06 §2`).
#[derive(Debug)]
pub enum Origin<'a, D> {
    /// Клиент на слушателе: идентичность — `hello.token`.
    Listener,
    /// Ядро набрало запись `D`: Noise уже доказал её PSK, токена в `hello` нет.
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

    /// Handshake на соединении, которое ядро набрало само (`dial`, пара в
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
    /// `MAX_HANDSHAKES` (и `MAX_HANDSHAKES_PER_NET` из одной сети). Сверх
    /// лимита новичок вытесняет старейшее соединение, ещё не приславшее
    /// `hello`: молчащие сокеты без токена лишь задерживают модули, а не
    /// запирают им вход.
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
                        log::debug!(ip:? = pass.ip; "silent connection evicted before hello");
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

/// Сеть пира для лимита до `welcome`: IPv4 — адрес, IPv6 — префикс /64,
/// который обычно целиком у одного хоста.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Net(IpAddr);

impl Net {
    fn of(ip: IpAddr) -> Self {
        match ip.to_canonical() {
            IpAddr::V4(v4) => Self(IpAddr::V4(v4)),
            IpAddr::V6(v6) => {
                let prefix = u128::from(v6) & !u128::from(u64::MAX);
                Self(IpAddr::V6(Ipv6Addr::from(prefix)))
            }
        }
    }
}

/// Места для соединений до `welcome`: всего и на одну сеть.
struct Gate {
    state: Mutex<GateState>,
}

#[derive(Default)]
struct GateState {
    next: u64,
    /// Места по возрасту: первое — старейшее.
    seats: BTreeMap<u64, Seat>,
}

struct Seat {
    net: Option<Net>,
    evict: CancellationToken,
    /// `hello` ещё не пришёл: место можно отдать новичку.
    silent: bool,
}

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Место новичку; при исчерпанном лимите — за счёт старейшего молчащего:
    /// из его же сети, если она выбрала свою долю, иначе любого.
    fn admit(self: &Arc<Self>, ip: Option<IpAddr>) -> Option<Pass> {
        let net = ip.map(Net::of);
        let kin = |seat: &Seat| net.is_some() && seat.net == net;
        let mut state = self.lock();
        let crowded =
            state.seats.values().filter(|&seat| kin(seat)).count() >= MAX_HANDSHAKES_PER_NET;
        if crowded || state.seats.len() >= MAX_HANDSHAKES {
            let victim = state
                .seats
                .iter()
                .find(|(_, seat)| seat.silent && (!crowded || kin(seat)))
                .map(|(&key, _)| key)?;
            if let Some(seat) = state.seats.remove(&victim) {
                seat.evict.cancel();
            }
        }

        let key = state.next;
        state.next += 1;
        let evict = CancellationToken::new();
        state.seats.insert(
            key,
            Seat {
                net,
                evict: evict.clone(),
                silent: true,
            },
        );
        Some(Pass {
            gate: self.clone(),
            key,
            ip,
            evict,
        })
    }
}

/// Место соединения в `Gate` до конца handshake.
struct Pass {
    gate: Arc<Gate>,
    key: u64,
    ip: Option<IpAddr>,
    evict: CancellationToken,
}

impl Pass {
    async fn evicted(&self) {
        self.evict.cancelled().await;
    }

    /// Пир прислал `hello`: впереди `authenticate`, его не прерывают.
    fn spoke(&self) {
        if let Some(seat) = self.gate.lock().seats.get_mut(&self.key) {
            seat.silent = false;
        }
    }
}

impl Drop for Pass {
    fn drop(&mut self) {
        self.gate.lock().seats.remove(&self.key);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn net(ip: &str) -> Net {
        Net::of(ip.parse().unwrap())
    }

    fn ip(ip: &str) -> Option<IpAddr> {
        Some(ip.parse().unwrap())
    }

    #[test]
    fn ipv6_hosts_share_a_net_by_64() {
        assert_eq!(net("2001:db8::1"), net("2001:db8::2"));
        assert_eq!(net("2001:db8::1"), net("2001:db8::ffff:ffff:ffff:ffff"));
        assert_ne!(net("2001:db8::1"), net("2001:db8:0:1::1"));
        assert_ne!(net("192.0.2.1"), net("192.0.2.2"));
        // IPv4 через dual-stack сокет — всё тот же адрес, а не одна /64 на всех.
        assert_eq!(net("::ffff:192.0.2.1"), net("192.0.2.1"));
        assert_ne!(net("::ffff:192.0.2.1"), net("::ffff:192.0.2.2"));
    }

    #[test]
    fn crowded_net_evicts_its_own_oldest() {
        let gate = Gate::new();
        let other = gate.admit(ip("192.0.2.9")).unwrap();
        let own: Vec<_> = (0..MAX_HANDSHAKES_PER_NET)
            .map(|n| gate.admit(ip(&format!("2001:db8::{n}"))).unwrap())
            .collect();
        let newcomer = gate.admit(ip("2001:db8::ff")).unwrap();

        assert!(own[0].evict.is_cancelled());
        assert!(own[1..].iter().all(|pass| !pass.evict.is_cancelled()));
        assert!(!other.evict.is_cancelled());
        assert!(!newcomer.evict.is_cancelled());
    }

    #[test]
    fn full_gate_evicts_the_oldest_silent_only() {
        let gate = Gate::new();
        let held: Vec<_> = (0..MAX_HANDSHAKES)
            .map(|n| gate.admit(ip(&format!("10.0.{n}.1"))).unwrap())
            .collect();
        held[0].spoke();

        let newcomer = gate.admit(ip("192.0.2.1")).unwrap();
        assert!(
            !held[0].evict.is_cancelled(),
            "authenticating is not evicted"
        );
        assert!(held[1].evict.is_cancelled());

        for pass in held.iter().chain([&newcomer]) {
            pass.spoke();
        }
        assert!(gate.admit(ip("192.0.2.2")).is_none());
        assert!(gate.admit(None).is_none());
    }
}
