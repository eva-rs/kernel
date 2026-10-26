//! ws/wss: один binary-message — один кадр; HTTP только для Upgrade.

use std::sync::Arc;

use futures::{SinkExt as _, StreamExt as _, future};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{
    Connector, WebSocketStream,
    tungstenite::{
        Message,
        error::{CapacityError, Error as WsError},
        handshake::server::{ErrorResponse, Request, Response},
        http::StatusCode,
        protocol::{CloseFrame, WebSocketConfig, frame::coding::CloseCode},
    },
};

use eva::url::Url;
use eva_protocol::wire::RpcError;

use crate::{
    config::{ClientTls, Config},
    transport::{
        Conn, MapErr, Outbound, TransportError, WS_PATH,
        meter::{Meter, Metered},
        tls,
    },
};

/// Потолок close-причины в байтах по RFC 6455.
const CLOSE_REASON_MAX: usize = 123;

fn ws_config(cfg: &Config) -> WebSocketConfig {
    let max = Some(cfg.max_frame());
    WebSocketConfig::default()
        .max_message_size(max)
        .max_frame_size(max)
}

#[allow(
    clippy::result_large_err,
    reason = "the signature is the handshake callback of the ws library"
)]
fn only_modules(req: &Request, resp: Response) -> Result<Response, ErrorResponse> {
    if req.uri().path() == WS_PATH {
        Ok(resp)
    } else {
        let mut reject = ErrorResponse::new(Some(format!("only {WS_PATH} is served")));
        *reject.status_mut() = StatusCode::NOT_FOUND;
        Err(reject)
    }
}

pub(crate) async fn accept<S>(
    stream: S,
    cfg: &Config,
    remote: String,
) -> Result<Conn, TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let meter = Meter::new();
    let ws = tokio_tungstenite::accept_hdr_async_with_config(
        Metered::new(stream, meter.clone()),
        only_modules,
        Some(ws_config(cfg)),
    )
    .await
    .map_err(classify)?;
    Ok(conn(ws, remote, meter))
}

pub(crate) async fn connect(
    target: &Url,
    cfg: &Config,
    tls: &ClientTls,
) -> Result<Conn, TransportError> {
    let mut url = target.clone();
    if matches!(url.path(), "" | "/") {
        url.set_path(WS_PATH);
    }

    let host = url
        .host_str()
        .ok_or_else(|| TransportError::Target(format!("{url} has no host")))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| TransportError::Target(format!("{url} has no port")))?;
    let socket = tokio::net::TcpStream::connect(format!("{host}:{port}"))
        .await
        .map_err(TransportError::io)?;
    socket.set_nodelay(true).map_err(TransportError::io)?;

    let connector = connector(&url, tls).await?;
    let meter = Meter::new();
    let (ws, _) = tokio_tungstenite::client_async_tls_with_config(
        url.as_str(),
        Metered::new(socket, meter.clone()),
        Some(ws_config(cfg)),
        connector,
    )
    .await
    .map_err(classify)?;

    Ok(conn(ws, url.to_string(), meter))
}

/// Для `wss` конфиг всегда свой: без него библиотека ws строит rustls на
/// провайдере процесса, которого у SDK может не быть.
async fn connector(url: &Url, tls: &ClientTls) -> Result<Option<Connector>, TransportError> {
    if url.scheme() != "wss" {
        return Ok(None);
    }
    let config = match &tls.extra_ca {
        Some(ca) => tls::client_config(ca).await?,
        None => tls::public_client_config()?,
    };
    Ok(Some(Connector::Rustls(config)))
}

fn conn<S>(ws: WebSocketStream<S>, remote: String, meter: Arc<Meter>) -> Conn
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (sink, mut source) = ws.split();

    let tx = MapErr::new(sink, classify).with(|out: Outbound| {
        future::ready(Ok::<_, TransportError>(match out {
            Outbound::Frame(frame) => Message::Binary(frame),
            Outbound::Close(reason) => Message::Close(Some(close_frame(reason))),
        }))
    });

    let rx = async_stream::stream! {
        while let Some(msg) = source.next().await {
            match msg {
                Ok(Message::Binary(frame)) => yield Ok(frame),
                Ok(Message::Close(_)) => break,
                Ok(Message::Text(_)) => {
                    yield Err(TransportError::Protocol("text message on a binary protocol".to_owned()));
                    break;
                }
                Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
                Err(e) => {
                    yield Err(classify(e));
                    break;
                }
            }
        }
    };

    Conn::metered(tx, rx, remote, meter)
}

fn close_frame(reason: Option<RpcError>) -> CloseFrame {
    let Some(reason) = reason else {
        return CloseFrame {
            code: CloseCode::Normal,
            reason: "".into(),
        };
    };

    let mut text = reason.to_string();
    let mut end = text.len().min(CLOSE_REASON_MAX);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);

    CloseFrame {
        code: CloseCode::Policy,
        reason: text.into(),
    }
}

fn classify(e: WsError) -> TransportError {
    match e {
        WsError::Capacity(CapacityError::MessageTooLong { .. }) => TransportError::TooLarge,
        WsError::Tls(e) => TransportError::Tls(e.to_string()),
        e => TransportError::io(e),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[tokio::test]
    async fn wss_always_gets_its_own_rustls_config() {
        let public = connector(&url("wss://soul.example"), &ClientTls::default())
            .await
            .unwrap();
        assert!(matches!(public, Some(Connector::Rustls(_))));

        let pinned = ClientTls {
            extra_ca: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ca.pem")),
        };
        let own_ca = connector(&url("wss://127.0.0.1:1"), &pinned).await.unwrap();
        assert!(matches!(own_ca, Some(Connector::Rustls(_))));
    }

    #[tokio::test]
    async fn plain_ws_has_no_tls() {
        let tls = ClientTls {
            extra_ca: Some(PathBuf::from("/nonexistent/ca.pem")),
        };
        assert!(
            connector(&url("ws://127.0.0.1:1"), &tls)
                .await
                .unwrap()
                .is_none()
        );
    }
}
