//! Сторона модуля: `hello` → `welcome` и `RpcSession`, реализующая все
//! трейты неймспейсов поверх RPC — код модуля совпадает с кодом ядра (`11 §6`).

use std::{
    marker::PhantomData,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use eva::{data, fut::Fut, url::Url};
use futures::{SinkExt as _, Stream, StreamExt as _};
use serde::{Serialize, de::DeserializeOwned};

use eva_core::{
    CallStep, Caller, Session, SessionMaker,
    service::{AuxFut, op_name},
};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::{ByteStream, Items, Response as OpResponse},
    wire::{
        ErrorCode, Frame, RpcError,
        frame::Request,
        handshake::{Hello, Welcome},
        method,
    },
};

use crate::{
    codec,
    config::{ClientTls, Config},
    module::{ModuleHandler, ModuleService, NoHandler},
    peer::{IdSeq, Peer, RemoteStream},
    transport::{self, Conn, Outbound},
};

/// Сколько клиент дожимает close в сокет после отказа handshake.
const CLOSE_GRACE: Duration = Duration::from_secs(5);

/// Настройки подключения модуля.
#[derive(Debug, Clone, Default)]
pub struct ClientOptions {
    pub config: Config,
    pub tls: ClientTls,
    /// При `hello.resume` — счётчик прошлой сессии.
    pub ids: IdSeq,
}

#[data(error)]
pub enum ConnectError {
    /// Сокет, TLS или Upgrade не поднялись.
    #[display("transport: {_0}")]
    Transport(String),
    /// Ядро ответило на `hello` отказом: `unauthorized`, `proto_version`, …
    #[display("rejected: {_0}")]
    Rejected(RpcError),
    /// Ядро нарушило handshake: кадр до `welcome`, молчание, мусор.
    #[display("handshake: {_0}")]
    Handshake(String),
    /// Негоден конфиг самого модуля (секрет токена не прочитался, `hello`
    /// не влез в `max_hello`): ядру ничего не ушло.
    #[display("config: {_0}")]
    Config(String),
}

struct ClientInner {
    peer: Peer,
    caller: Caller,
    welcome: Welcome,
}

impl Drop for ClientInner {
    fn drop(&mut self) {
        self.peer
            .close(RpcError::new(ErrorCode::Unavailable, "client closed"));
    }
}

/// Подключённый модуль. Последний клон (вместе с сессиями и потоками)
/// закрывает соединение.
#[derive(Clone)]
pub struct RpcClient {
    inner: Arc<ClientInner>,
}

impl std::fmt::Debug for RpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcClient")
            .field("caller", &self.inner.caller)
            .field("session", &self.inner.welcome.session_id)
            .finish_non_exhaustive()
    }
}

impl RpcClient {
    /// Подключение без обработчиков ядро → модуль: такие вызовы получат `unknown_method`.
    pub async fn connect(target: &Url, hello: Hello) -> Result<(Self, Welcome), ConnectError> {
        Self::connect_with(target, hello, ClientOptions::default(), NoHandler).await
    }

    /// Сокет, TLS и Upgrade укладываются в `handshake_timeout`: иначе путь,
    /// пропавший посреди handshake, держал бы реконнект вечно.
    pub async fn connect_with<H: ModuleHandler>(
        target: &Url,
        hello: Hello,
        options: ClientOptions,
        handler: H,
    ) -> Result<(Self, Welcome), ConnectError> {
        let request = HelloRequest::encode(&hello, &options)?;
        let connect = transport::connect(target, &options.config, &options.tls);
        let conn = tokio::time::timeout(*options.config.handshake_timeout, connect)
            .await
            .map_err(|_| ConnectError::Transport("transport connect timed out".to_owned()))?
            .map_err(|e| ConnectError::Transport(e.to_string()))?;
        Self::establish(conn, request, options, handler).await
    }

    /// Handshake клиента поверх готового соединения (в том числе `Conn::pair`).
    pub async fn handshake<H: ModuleHandler>(
        conn: Conn,
        hello: Hello,
        options: ClientOptions,
        handler: H,
    ) -> Result<(Self, Welcome), ConnectError> {
        let request = HelloRequest::encode(&hello, &options)?;
        Self::establish(conn, request, options, handler).await
    }

    /// Handshake сервиса на соединении, которое набрало ядро
    /// (`Incoming::secure`): `hello` без токена, ограничен лишь `max_frame`.
    pub async fn answer<H: ModuleHandler>(
        conn: Conn,
        hello: Hello,
        options: ClientOptions,
        handler: H,
    ) -> Result<(Self, Welcome), ConnectError> {
        if hello.token.is_some() {
            return Err(ConnectError::Config(
                "a dialed module sends no token".to_owned(),
            ));
        }
        let request = HelloRequest::encode_dialed(&hello, &options)?;
        Self::establish(conn, request, options, handler).await
    }

    async fn establish<H: ModuleHandler>(
        mut conn: Conn,
        request: HelloRequest,
        options: ClientOptions,
        handler: H,
    ) -> Result<(Self, Welcome), ConnectError> {
        let ClientOptions { config, ids, .. } = options;

        let welcome = match hello_exchange(&mut conn, request, &config).await {
            Ok(welcome) => welcome,
            Err((error, reason)) => {
                shut(conn, reason).await;
                return Err(error);
            }
        };

        let caller = Caller::from_kind(welcome.kind, welcome.module.clone());
        let service = ModuleService::new(handler, config.stream_buffer());
        let peer = Peer::spawn(conn, config, ids, service);

        let client = Self {
            inner: Arc::new(ClientInner {
                peer,
                caller,
                welcome: welcome.clone(),
            }),
        };
        Ok((client, welcome))
    }

    pub fn peer(&self) -> &Peer {
        &self.inner.peer
    }

    pub fn welcome(&self) -> &Welcome {
        &self.inner.welcome
    }

    /// Caller, которого решил токен; другого клиент не получит.
    pub fn caller(&self) -> &Caller {
        &self.inner.caller
    }

    pub fn session(&self) -> Session<RpcSession> {
        Session::new(
            RpcSession {
                inner: self.inner.clone(),
            },
            self.inner.caller.clone(),
        )
    }

    /// Ждёт закрытия соединения и отдаёт причину.
    pub async fn closed(&self) -> RpcError {
        self.inner.peer.closed().await
    }
}

impl SessionMaker for RpcClient {
    type Session = RpcSession;

    fn make_session(&self, caller: Caller) -> impl AuxFut<Session<RpcSession>> {
        let session = if caller == self.inner.caller {
            Ok(self.session())
        } else {
            Err(Aux::forbidden(format_args!(
                "this connection acts as {}",
                self.inner.caller.label()
            )))
        };
        async move { session }
    }
}

type HandshakeFailure = (ConnectError, Option<RpcError>);

/// Кадр `hello`, собранный до сети: не влезший в `max_hello` ядро всё
/// равно отвергло бы, поэтому он не отправляется.
struct HelloRequest {
    id: u64,
    raw: bytes::Bytes,
}

impl HelloRequest {
    fn encode(hello: &Hello, options: &ClientOptions) -> Result<Self, ConnectError> {
        Self::capped(hello, options, "max_hello", options.config.max_hello())
    }

    fn encode_dialed(hello: &Hello, options: &ClientOptions) -> Result<Self, ConnectError> {
        Self::capped(hello, options, "max_frame", options.config.max_frame())
    }

    fn capped(
        hello: &Hello,
        options: &ClientOptions,
        name: &str,
        max: usize,
    ) -> Result<Self, ConnectError> {
        let id = options.ids.next();
        let params = codec::encode(hello).map_err(|e| ConnectError::Config(e.to_string()))?;
        let raw = codec::encode_frame(&Frame::Request(Request {
            id,
            method: method::HELLO.to_owned(),
            params,
        }))
        .map_err(|e| ConnectError::Config(e.to_string()))?;
        if raw.len() > max {
            return Err(ConnectError::Config(format!(
                "hello of {} bytes exceeds {name} of {max} bytes",
                raw.len()
            )));
        }
        Ok(Self { id, raw })
    }
}

async fn hello_exchange(
    conn: &mut Conn,
    request: HelloRequest,
    config: &Config,
) -> Result<Welcome, HandshakeFailure> {
    let protocol = |message: String| {
        let reason = RpcError::invalid_params(&message);
        (ConnectError::Handshake(message), Some(reason))
    };

    let HelloRequest { id, raw } = request;
    if let Err(e) = conn.tx.send(Outbound::Frame(raw)).await {
        // Ядро рвёт сокет, не дочитав крупный `hello`, но причину кладёт в него раньше.
        let reason = tokio::time::timeout(*config.handshake_timeout, close_reason(conn)).await;
        let error = match reason {
            Ok(Some(reason)) => ConnectError::Rejected(reason),
            _ => ConnectError::Transport(e.to_string()),
        };
        return Err((error, None));
    }

    let first = tokio::time::timeout(*config.handshake_timeout, conn.rx.next())
        .await
        .map_err(|_| protocol("no welcome within handshake_timeout".to_owned()))?;
    let raw = match first {
        None => {
            return Err((
                ConnectError::Transport("closed before welcome".to_owned()),
                None,
            ));
        }
        Some(Err(e)) => return Err((ConnectError::Transport(e.to_string()), None)),
        Some(Ok(raw)) => raw,
    };

    match codec::decode_frame(&raw) {
        Ok(Frame::Response(response)) if response.id == id => match response.into_result() {
            Ok(welcome) => {
                codec::decode(&welcome).map_err(|e| protocol(format!("malformed welcome: {e}")))
            }
            Err(error) => Err((ConnectError::Rejected(error), None)),
        },
        // Отказ до разбора `hello` (без id запроса) ядро шлёт уведомлением `close`.
        Ok(Frame::Notify(notify)) if notify.method == method::CLOSE => {
            let reason = codec::decode::<RpcError>(&notify.params)
                .unwrap_or_else(|e| codec::unavailable(format_args!("closed by kernel: {e}")));
            Err((ConnectError::Rejected(reason), None))
        }
        Ok(_) => Err(protocol("a frame before welcome".to_owned())),
        Err(e) => Err(protocol(format!("malformed frame: {e}"))),
    }
}

/// Причина из `notify close`, если ядро успело её прислать до обрыва.
async fn close_reason(conn: &mut Conn) -> Option<RpcError> {
    while let Some(Ok(raw)) = conn.rx.next().await {
        if let Ok(Frame::Notify(notify)) = codec::decode_frame(&raw)
            && notify.method == method::CLOSE
        {
            return codec::decode(&notify.params).ok();
        }
    }
    None
}

async fn shut(mut conn: Conn, reason: Option<RpcError>) {
    let close = async {
        _ = conn.tx.send(Outbound::Close(reason)).await;
        _ = conn.tx.close().await;
    };
    _ = tokio::time::timeout(CLOSE_GRACE, close).await;
}

/// Сессия поверх RPC: каждая операция — запрос `<ns>.<op>` этого соединения.
#[derive(Clone)]
pub struct RpcSession {
    inner: Arc<ClientInner>,
}

impl std::fmt::Debug for RpcSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcSession")
            .field("session", &self.inner.welcome.session_id)
            .finish_non_exhaustive()
    }
}

/// Ошибка с провода типом операции: `data` → `Generic<E>`, иначе класс по коду (`11 §6`).
pub fn from_rpc<E: DeserializeOwned>(error: RpcError) -> Generic<E> {
    if let Some(generic) = error
        .data
        .as_ref()
        .and_then(|data| codec::decode::<Generic<E>>(data).ok())
    {
        return generic;
    }
    Generic::Aux(error.into_aux())
}

fn invalid<E>(what: &str, e: impl std::fmt::Display) -> Generic<E> {
    Generic::Aux(Aux::Invalid(format!("{what}: {e}")))
}

struct Target {
    inner: Arc<ClientInner>,
    method: String,
}

/// Шаг чтения и мутации.
pub struct Unary<A, O, E> {
    target: Target,
    _types: PhantomData<fn(A) -> (O, E)>,
}

impl<A, O, E> CallStep<A> for Unary<A, O, E>
where
    A: Serialize,
    O: DeserializeOwned + Send + 'static,
    E: DeserializeOwned + Send + 'static,
{
    type Output = OpResponse<O, E>;

    fn call(&mut self, args: A) -> impl Fut<Output = Self::Output> {
        let client = self.target.inner.clone();
        let method = self.target.method.clone();
        let params = codec::encode(&args);
        async move {
            let params = params.map_err(|e| invalid("arguments", e))?;
            let raw = client.peer.call(&method, params).await.map_err(from_rpc)?;
            codec::decode(&raw).map_err(|e| invalid("malformed result", e))
        }
    }
}

/// Шаг потока к вызывающему: `Ok` — элементы из кадров потока.
pub struct Streaming<A, T, E, O> {
    wrap: fn(Items<T, E>) -> O,
    target: Target,
    _types: PhantomData<fn(A) -> (T, E)>,
}

impl<A, T, E, O> CallStep<A> for Streaming<A, T, E, O>
where
    A: Serialize,
    T: DeserializeOwned + Send + 'static,
    E: DeserializeOwned + Send + 'static,
{
    type Output = OpResponse<O, E>;

    fn call(&mut self, args: A) -> impl Fut<Output = Self::Output> {
        let wrap = self.wrap;
        let inner = self.target.inner.clone();
        let method = self.target.method.clone();
        let params = codec::encode(&args);
        async move {
            let params = params.map_err(|e| invalid("arguments", e))?;
            let remote = inner.peer.open(&method, params).await.map_err(from_rpc)?;
            let items: Items<T, E> = Box::pin(Decoded {
                remote,
                _client: inner,
                done: false,
                _types: PhantomData,
            });
            Ok(wrap(items))
        }
    }
}

/// Элементы потока типом операции; держит соединение, пока жив.
struct Decoded<T, E> {
    remote: RemoteStream,
    _client: Arc<ClientInner>,
    done: bool,
    _types: PhantomData<fn() -> (T, E)>,
}

impl<T: DeserializeOwned, E: DeserializeOwned> Stream for Decoded<T, E> {
    type Item = OpResponse<T, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        let item = match this.remote.poll_next_unpin(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(None) => return Poll::Ready(None),
            Poll::Ready(Some(Ok(raw))) => {
                codec::decode(&raw).map_err(|e| invalid("malformed item", e))
            }
            Poll::Ready(Some(Err(error))) => Err(from_rpc(error)),
        };
        this.done = item.is_err();
        Poll::Ready(Some(item))
    }
}

/// Шаг загрузки: `split` делит `Args` на `Meta` для `params` и тело, которое
/// уходит чанками после запроса.
pub struct Uploading<A, M, O, E> {
    target: Target,
    split: fn(A) -> (M, ByteStream),
    _types: PhantomData<fn() -> (O, E)>,
}

impl<A, M, O, E> CallStep<A> for Uploading<A, M, O, E>
where
    M: Serialize,
    O: DeserializeOwned + Send + 'static,
    E: DeserializeOwned + Send + 'static,
{
    type Output = OpResponse<O, E>;

    fn call(&mut self, args: A) -> impl Fut<Output = Self::Output> {
        let (meta, body) = (self.split)(args);
        let client = self.target.inner.clone();
        let method = self.target.method.clone();
        let params = codec::encode(&meta);
        async move {
            let params = params.map_err(|e| invalid("arguments", e))?;
            let raw = client
                .peer
                .upload(&method, params, body)
                .await
                .map_err(from_rpc)?;
            codec::decode(&raw).map_err(|e| invalid("malformed result", e))
        }
    }
}

macro_rules! step {
    ([] $target:ident $Args:ty, $Ok:ty) => {
        Unary {
            target: $target,
            _types: PhantomData,
        }
    };
    ([mutating] $target:ident $Args:ty, $Ok:ty) => {
        step!([] $target $Args, $Ok)
    };
    ([stream] $target:ident $Args:ty, $Ok:ty) => {
        Streaming {
            wrap: <$Ok>::from,
            target: $target,
            _types: PhantomData,
        }
    };
    ([upload] $target:ident $Args:ty, $Ok:ty) => {{
        type Args = $Args;

        Uploading {
            target: $target,
            split: |Args { meta, body }| (meta, body),
            _types: PhantomData,
        }
    }};
}

macro_rules! client_impls {
    ($(
        $segment:ident {$(
            $accessor:ident : $Trait:ident {
                trait: $TraitPath:path,
                client: $Client:path,
                $(
                $(#[$flag:ident])*
                $method:ident = $endpoint:ident {
                    args: $Args:ty,
                    ok: $Ok:ty,
                    err: $Err:ty,
                    output: $Output:ty,
                    docs: [$($doc:literal),*],
                }
            )*}
        )*}
    )*) => {$($(
        impl $TraitPath for RpcSession {$(
            fn $method(&mut self) -> impl CallStep<$Args, Output = $Output> {
                let target = self.target(stringify!($segment), stringify!($endpoint));
                step!([$($flag)*] target $Args, $Ok)
            }
        )*}
    )*)*};
}

impl RpcSession {
    fn target(&self, segment: &str, endpoint: &'static str) -> Target {
        Target {
            inner: self.inner.clone(),
            method: format!("{segment}.{}", op_name(endpoint)),
        }
    }

    pub fn caller(&self) -> &Caller {
        &self.inner.caller
    }
}

eva_core::for_each_op!(client_impls);
