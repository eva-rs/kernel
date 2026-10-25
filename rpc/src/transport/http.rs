//! HTTP-мост JSON-RPC для клиентов (`06 §1`): ещё одно кадрирование запроса
//! и ответа поверх того же диспетчера и `Policy`. `POST /v1/rpc` с конвертом
//! JSON-RPC 2.0 (`envelope`) в JSON или msgpack (`format`);
//! `Authorization: Bearer <token>` вместо `hello`. Каждый запрос — одноразовая
//! сессия клиента: без строки `modules`, окна идемпотентности и `resume`;
//! потоки, загрузки и события — только по ws.

use std::{
    convert::Infallible,
    fmt,
    net::SocketAddr,
    pin::pin,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use bytes::Bytes;
use futures::future::BoxFuture;
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt as _, Full, LengthLimitError, Limited};
use hyper::{
    body::{Body as _, Incoming},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::{Serialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::TlsAcceptor;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use eva::handling::CallStep;
use eva_core::{Caller, Session};
use eva_protocol::{
    errors::HasCode,
    requests::Response as OpResponse,
    wire::{ErrorCode, RpcError, handshake::Token},
};

use crate::{
    codec,
    config::{Config, Cors},
    gate::{Gate, Pass},
    transport::ACCEPT_BACKOFF,
};

use self::envelope::{Id, Message, Params, Rejection, Request as Envelope};

pub use self::format::Format;

pub mod envelope;
pub mod format;

/// Единственный путь моста.
pub const RPC_PATH: &str = "/v1/rpc";

/// Сколько остановка ждёт ответов в полёте.
const DRAIN: Duration = Duration::from_secs(5);

/// Привязанный слушатель моста: TCP, у `https` — под TLS.
pub(crate) struct Bound {
    listener: TcpListener,
    acceptor: Option<TlsAcceptor>,
    cors: Cors,
}

impl Bound {
    pub(crate) fn new(listener: TcpListener, acceptor: Option<TlsAcceptor>, cors: Cors) -> Self {
        Self {
            listener,
            acceptor,
            cors,
        }
    }

    pub(crate) fn local_addr(&self) -> Option<SocketAddr> {
        self.listener.local_addr().ok()
    }

    fn loopback(&self) -> bool {
        self.local_addr()
            .is_some_and(|addr| addr.ip().is_loopback())
    }
}

impl Cors {
    /// Без списка: любой origin у слушателя на loopback, никакой у прочих.
    pub fn allows(&self, origin: &str, loopback: bool) -> bool {
        if self.0.is_empty() {
            loopback
        } else {
            self.0
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(origin))
        }
    }
}

/// Сторона диспетчера: токен → caller и исполнение одного запроса конверта.
pub(crate) trait Exchange: Send + Sync + 'static {
    fn identify<'a>(&'a self, token: &'a Token) -> BoxFuture<'a, Result<Caller, RpcError>>;

    /// Тело ответа на один запрос конверта.
    fn dispatch<'a>(
        &'a self,
        caller: Caller,
        method: &'a str,
        call: Call,
    ) -> BoxFuture<'a, Vec<u8>>;
}

/// Запрос конверта до разбора `Args`: `id` и кодек ответа возвращаются в конверт.
pub(crate) struct Call {
    pub format: Format,
    pub id: Id,
    pub params: Params,
}

/// Тело ответа и, при отказе, его класс и текст для лога края.
pub(crate) struct Reply {
    pub body: Vec<u8>,
    pub failure: Option<(ErrorCode, String)>,
}

/// Шаг операции под конвертом: `Args` из кодека запроса, ответ — в кодек ответа.
pub(crate) type Handler<S> = fn(Session<S>, Call) -> BoxFuture<'static, Reply>;

pub(crate) async fn unary<A, O, E>(
    mut step: impl CallStep<A, Output = OpResponse<O, E>>,
    call: Call,
) -> Reply
where
    A: DeserializeOwned,
    O: Serialize,
    E: Serialize + HasCode + fmt::Display,
{
    let args = match call.params.decode::<A>() {
        Ok(args) => args,
        Err(e) => return call.failed(&codec::bad_params(e)),
    };
    match step.call(args).await {
        Ok(ok) => Reply {
            body: envelope::success(call.format, &call.id, &ok),
            failure: None,
        },
        Err(error) => call.typed(&error),
    }
}

impl Call {
    /// Ошибка транспорта или моста: без типизированной `data.error`.
    pub(crate) fn failed(&self, error: &RpcError) -> Reply {
        Reply {
            body: envelope::transport(self.format, &self.id, error),
            failure: Some((error.code, error.message.clone())),
        }
    }

    /// Ошибка операции (`Generic<E>`) или `Aux`: как `codec::typed` на проводе.
    pub(crate) fn typed<E: Serialize + HasCode + fmt::Display>(&self, error: &E) -> Reply {
        Reply {
            body: envelope::typed(self.format, &self.id, error),
            failure: Some((error.code(), error.to_string())),
        }
    }
}

/// Цикл приёма моста: соединение — в своей задаче, место в `Gate` — до
/// первого принятого токена; остановка закрывает соединения мягко и ждёт
/// ответы в полёте не дольше `DRAIN`.
pub(crate) async fn serve(
    bound: &Bound,
    cfg: &Config,
    gate: &Arc<Gate>,
    shutdown: impl Future<Output = ()>,
    exchange: Arc<dyn Exchange>,
) {
    let bridge = Arc::new(Bridge {
        exchange,
        cors: bound.cors.clone(),
        loopback: bound.loopback(),
        max_body: cfg.max_frame(),
        max_batch: cfg.max_batch(),
    });
    let handshake: Duration = *cfg.handshake_timeout;
    let stop = CancellationToken::new();
    let connections = TaskTracker::new();
    let mut shutdown = pin!(shutdown);

    loop {
        let accepted = tokio::select! {
            () = &mut shutdown => break,
            accepted = bound.listener.accept() => accepted,
        };
        let (stream, addr) = match accepted {
            Ok(accepted) => accepted,
            Err(e) => {
                log::warn!(error:% = e; "accept failed");
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
        };
        let Some(pass) = gate.admit(Some(addr.ip())) else {
            log::debug!(ip:% = addr.ip(); "too many connections before a token, dropped");
            continue;
        };
        connections.spawn(connection(
            stream,
            addr,
            bound.acceptor.clone(),
            handshake,
            bridge.clone(),
            pass,
            stop.clone(),
        ));
    }

    stop.cancel();
    connections.close();
    if tokio::time::timeout(DRAIN, connections.wait())
        .await
        .is_err()
    {
        log::warn!("http connections did not close in time");
    }
}

async fn connection(
    stream: TcpStream,
    addr: SocketAddr,
    acceptor: Option<TlsAcceptor>,
    handshake: Duration,
    bridge: Arc<Bridge>,
    pass: Pass,
    stop: CancellationToken,
) {
    _ = stream.set_nodelay(true);
    let remote: Arc<str> = addr.to_string().into();
    let Some(acceptor) = acceptor else {
        return talk(stream, remote, handshake, bridge, pass, stop).await;
    };
    match tokio::time::timeout(handshake, acceptor.accept(stream)).await {
        Ok(Ok(stream)) => talk(stream, remote, handshake, bridge, pass, stop).await,
        Ok(Err(e)) => log::debug!(remote = &*remote, error:% = e; "tls handshake failed"),
        Err(_) => log::debug!(remote = &*remote; "tls handshake timed out"),
    }
}

/// HTTP/1.1 с keep-alive: заголовки каждого запроса приходят за
/// `handshake_timeout`, иначе соединение закрывается; молчащее до первого
/// принятого токена соединение вытесняет `Gate`.
async fn talk<S>(
    stream: S,
    remote: Arc<str>,
    handshake: Duration,
    bridge: Arc<Bridge>,
    pass: Pass,
    stop: CancellationToken,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let evicted = pass.evicted();
    let seat = Arc::new(Seat(Mutex::new(Some(pass))));
    let service = {
        let remote = remote.clone();
        service_fn(move |request| {
            let (bridge, seat, remote) = (bridge.clone(), seat.clone(), remote.clone());
            async move { Ok::<_, Infallible>(bridge.handle(request, &seat, &remote).await) }
        })
    };

    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(handshake);
    let mut conn = pin!(builder.serve_connection(TokioIo::new(stream), service));
    let mut evicted = pin!(evicted);
    let mut stopping = false;
    loop {
        tokio::select! {
            result = &mut conn => {
                if let Err(e) = result {
                    log::debug!(remote = &*remote, error:% = e; "http connection failed");
                }
                return;
            }
            () = &mut evicted => {
                log::debug!(remote = &*remote; "silent http connection evicted");
                return;
            }
            () = stop.cancelled(), if !stopping => {
                stopping = true;
                conn.as_mut().graceful_shutdown();
            }
        }
    }
}

/// Место соединения в `Gate`, пока оно не предъявило принятый токен.
struct Seat(Mutex<Option<Pass>>);

impl Seat {
    fn proven(&self) {
        drop(self.0.lock().unwrap_or_else(PoisonError::into_inner).take());
    }
}

struct Bridge {
    exchange: Arc<dyn Exchange>,
    cors: Cors,
    loopback: bool,
    max_body: usize,
    max_batch: usize,
}

impl Bridge {
    async fn handle(
        &self,
        request: Request<Incoming>,
        seat: &Seat,
        remote: &str,
    ) -> Response<Full<Bytes>> {
        let (parts, body) = request.into_parts();
        let headers = &parts.headers;
        // Кодек ответа известен до разбора тела: отказы моста уходят в нём же.
        let requested = header_str(headers, &header::CONTENT_TYPE).and_then(Format::from_mime_type);
        let format = response_format(headers, requested.unwrap_or(Format::Json));
        let origin = match self.origin(headers) {
            Ok(origin) => origin,
            Err(()) => {
                let replier = Replier {
                    format,
                    origin: None,
                };
                let error = RpcError::new(ErrorCode::Forbidden, "origin not allowed");
                return replier.failed(StatusCode::FORBIDDEN, &error);
            }
        };
        let replier = Replier { format, origin };

        if parts.uri.path() != RPC_PATH {
            let message = format!("only POST {RPC_PATH} is served");
            return replier.rejected(StatusCode::NOT_FOUND, &message);
        }
        match parts.method {
            Method::POST => {}
            Method::OPTIONS => return replier.preflight(),
            _ => return replier.rejected(StatusCode::METHOD_NOT_ALLOWED, "only POST is served"),
        }
        let Some(requested) = requested else {
            return replier.rejected(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Content-Type must be application/json or application/msgpack",
            );
        };

        let token = match bearer(headers) {
            Ok(token) => token,
            Err(message) => {
                return replier.unauthorized(&RpcError::new(ErrorCode::Unauthorized, message));
            }
        };
        let caller = match self.exchange.identify(&token).await {
            Ok(caller) => caller,
            Err(error) => {
                log::info!(
                    remote,
                    code:% = error.code,
                    error = codec::shown(&error.message).as_str();
                    "http request refused"
                );
                return match error.code {
                    ErrorCode::Unauthorized => replier.unauthorized(&error),
                    _ => replier.failed(StatusCode::INTERNAL_SERVER_ERROR, &error),
                };
            }
        };
        seat.proven();

        let too_large = || {
            let message = format!("body exceeds max_frame of {} bytes", self.max_body);
            replier.rejected(StatusCode::PAYLOAD_TOO_LARGE, &message)
        };
        if body
            .size_hint()
            .upper()
            .is_some_and(|declared| declared > self.max_body as u64)
        {
            return too_large();
        }
        let body = match Limited::new(body, self.max_body).collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(e) if e.is::<LengthLimitError>() => return too_large(),
            Err(e) => {
                let message = format!("body not read: {e}");
                return replier.rejected(StatusCode::BAD_REQUEST, &message);
            }
        };

        let message = match envelope::parse(requested, &body, self.max_batch) {
            Ok(message) => message,
            Err(rejection) => return replier.body(rejection.answer(format)),
        };
        let response = match message {
            Message::Single(request) => self.run(&caller, format, request).await,
            Message::Batch(requests) => {
                let mut responses = Vec::with_capacity(requests.len());
                for request in requests {
                    let response = match request {
                        Ok(request) => self.run(&caller, format, request).await,
                        Err(rejection) => Some(rejection.answer(format)),
                    };
                    responses.extend(response);
                }
                (!responses.is_empty()).then(|| envelope::batch(format, &responses))
            }
        };
        match response {
            Some(body) => replier.body(body),
            None => replier.empty(StatusCode::NO_CONTENT),
        }
    }

    /// `None` — ответа не ждут (уведомление).
    async fn run(&self, caller: &Caller, format: Format, request: Envelope<'_>) -> Option<Vec<u8>> {
        let notification = request.id.is_none();
        let call = Call {
            format,
            id: request.id.unwrap_or(Id::Null),
            params: request.params,
        };
        let body = self
            .exchange
            .dispatch(caller.clone(), &request.method, call)
            .await;
        (!notification).then_some(body)
    }

    /// `Origin` запроса, если ему можно отвечать; без `Origin` (не браузер)
    /// CORS не нужен.
    fn origin(&self, headers: &HeaderMap) -> Result<Option<HeaderValue>, ()> {
        let Some(origin) = headers.get(header::ORIGIN) else {
            return Ok(None);
        };
        let allowed = origin
            .to_str()
            .is_ok_and(|origin| self.cors.allows(origin, self.loopback));
        if allowed {
            Ok(Some(origin.clone()))
        } else {
            Err(())
        }
    }
}

/// Ответы одного запроса: кодек тела и, при разрешённом `Origin`, CORS.
struct Replier {
    format: Format,
    origin: Option<HeaderValue>,
}

impl Replier {
    fn build(
        &self,
        status: StatusCode,
        body: Option<Vec<u8>>,
        extra: &[(header::HeaderName, &'static str)],
    ) -> Response<Full<Bytes>> {
        let mut response = Response::builder().status(status);
        if body.is_some() {
            response = response.header(header::CONTENT_TYPE, self.format.mime_type());
        }
        if let Some(origin) = &self.origin {
            response = response
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone())
                .header(header::VARY, "Origin");
        }
        for (name, value) in extra {
            response = response.header(name.clone(), *value);
        }
        response
            .body(Full::new(Bytes::from(body.unwrap_or_default())))
            // Все заголовки свои или уже прошли `HeaderValue`: сборка не падает.
            .unwrap_or_else(|_| Response::new(Full::default()))
    }

    fn body(&self, body: Vec<u8>) -> Response<Full<Bytes>> {
        self.build(StatusCode::OK, Some(body), &[])
    }

    fn empty(&self, status: StatusCode) -> Response<Full<Bytes>> {
        self.build(status, None, &[])
    }

    /// Тело не стало запросом: `invalid request` с `id: null`.
    fn rejected(&self, status: StatusCode, message: &str) -> Response<Full<Bytes>> {
        let body = Rejection::invalid(message).answer(self.format);
        let allow =
            (status == StatusCode::METHOD_NOT_ALLOWED).then_some((header::ALLOW, "POST, OPTIONS"));
        self.build(status, Some(body), allow.as_slice())
    }

    fn failed(&self, status: StatusCode, error: &RpcError) -> Response<Full<Bytes>> {
        let body = envelope::transport(self.format, &Id::Null, error);
        self.build(status, Some(body), &[])
    }

    fn unauthorized(&self, error: &RpcError) -> Response<Full<Bytes>> {
        let body = envelope::transport(self.format, &Id::Null, error);
        self.build(
            StatusCode::UNAUTHORIZED,
            Some(body),
            &[(header::WWW_AUTHENTICATE, "Bearer")],
        )
    }

    fn preflight(&self) -> Response<Full<Bytes>> {
        self.build(
            StatusCode::NO_CONTENT,
            None,
            &[
                (header::ALLOW, "POST, OPTIONS"),
                (header::ACCESS_CONTROL_ALLOW_METHODS, "POST, OPTIONS"),
                (
                    header::ACCESS_CONTROL_ALLOW_HEADERS,
                    "Authorization, Content-Type, Accept",
                ),
                (header::ACCESS_CONTROL_MAX_AGE, "86400"),
            ],
        )
    }
}

fn header_str<'h>(headers: &'h HeaderMap, name: &header::HeaderName) -> Option<&'h str> {
    headers.get(name)?.to_str().ok()
}

/// Кодек ответа: предпочтение `Accept`, иначе кодек запроса.
fn response_format(headers: &HeaderMap, fallback: Format) -> Format {
    header_str(headers, &header::ACCEPT)
        .and_then(|accept| Format::negotiate(accept, fallback))
        .unwrap_or(fallback)
}

/// Токен из `Authorization: Bearer <token>`.
fn bearer(headers: &HeaderMap) -> Result<Token, &'static str> {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Err("missing Authorization: Bearer <token>");
    };
    let value = value
        .to_str()
        .map_err(|_| "Authorization is not visible ascii")?;
    let (scheme, token) = value
        .split_once(' ')
        .ok_or("Authorization must be `Bearer <token>`")?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err("Authorization scheme must be Bearer");
    }
    let token = token.trim();
    if token.is_empty() {
        return Err("empty bearer token");
    }
    Ok(Token::from(token))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                header::HeaderName::from_static(name),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn bearer_token_is_the_only_scheme() {
        let token = bearer(&headers(&[("authorization", "Bearer  t0ken ")])).unwrap();
        assert_eq!(token.expose(), "t0ken");
        assert!(bearer(&headers(&[("authorization", "bearer x")])).is_ok());
        assert!(bearer(&headers(&[])).is_err());
        assert!(bearer(&headers(&[("authorization", "Basic dXNlcg==")])).is_err());
        assert!(bearer(&headers(&[("authorization", "Bearer")])).is_err());
        assert!(bearer(&headers(&[("authorization", "Bearer ")])).is_err());
    }

    #[test]
    fn cors_list_or_loopback() {
        let any = Cors::default();
        assert!(any.allows("http://localhost:3000", true));
        assert!(!any.allows("http://localhost:3000", false));

        let listed = Cors(vec![String::from("https://app.example")]);
        assert!(listed.allows("https://app.example", false));
        assert!(listed.allows("HTTPS://APP.EXAMPLE", true));
        assert!(!listed.allows("https://evil.example", true));
    }

    #[test]
    fn response_codec_follows_accept_then_request() {
        let json = headers(&[("accept", "application/json")]);
        assert_eq!(response_format(&json, Format::Msgpack), Format::Json);
        let any = headers(&[("accept", "*/*")]);
        assert_eq!(response_format(&any, Format::Msgpack), Format::Msgpack);
        let html = headers(&[("accept", "text/html")]);
        assert_eq!(response_format(&html, Format::Json), Format::Json);
        assert_eq!(
            response_format(&HeaderMap::new(), Format::Msgpack),
            Format::Msgpack
        );
    }
}
