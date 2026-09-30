//! Транспорты (`06 §1`): клиенты звонят ядру по uds (`u32 BE` длина + кадр),
//! ws и wss (один binary-message — один кадр); ядро набирает сервисы по uds
//! и tcp под Noise (`service`). Выше транспорта все они — `Conn`: поток сырых
//! кадров туда и обратно.

use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use eva::{data, url::Url};
use futures::{Sink, SinkExt as _, Stream, StreamExt as _};
use tokio::net::{TcpListener, UnixListener};
use tokio_rustls::TlsAcceptor;

use eva_protocol::wire::RpcError;

use crate::config::{ClientTls, Config, Listen};

use self::{
    activation::{Bound, Inherited},
    meter::Meter,
};

pub mod activation;
pub mod framed;
pub(crate) mod meter;
pub mod noise;
pub mod service;
pub mod tls;
pub mod ws;

pub use self::service::{Incoming, ServiceListener, dial};

/// Путь Upgrade у ws/wss.
pub const WS_PATH: &str = "/v1/modules";

#[data(error, not(serde, JsonSchema))]
pub enum TransportError {
    /// Кадр больше `max_frame`.
    #[display("frame exceeds max_frame")]
    TooLarge,
    #[display("{_0}")]
    Io(String),
    /// Пир нарушил кадрирование: текст вместо binary, неверный путь Upgrade.
    #[display("{_0}")]
    Protocol(String),
    #[display("unsupported target {_0}")]
    Target(String),
    #[display("tls: {_0}")]
    Tls(String),
    /// Noise не сошёлся: чужой PSK, не тот пир, битое сообщение.
    #[display("noise: {_0}")]
    Noise(String),
    /// Сокеты менеджера служб не сходятся с конфигом.
    #[display("socket activation: {_0}")]
    Activation(String),
}

impl TransportError {
    pub(crate) fn io(e: impl fmt::Display) -> Self {
        Self::Io(e.to_string())
    }
}

/// Что уходит в сокет: кадр либо просьба закрыть с причиной.
#[derive(Debug, Clone)]
pub enum Outbound {
    Frame(bytes::Bytes),
    /// ws передаёт причину close-кадром; uds просто закрывается.
    Close(Option<RpcError>),
}

pub(crate) type Tx = Pin<Box<dyn Sink<Outbound, Error = TransportError> + Send>>;
pub(crate) type Rx = Pin<Box<dyn Stream<Item = Result<bytes::Bytes, TransportError>> + Send>>;

pin_project_lite::pin_project! {
    /// Перевод ошибок синка, переживающий повторный опрос: `sink_map_err`
    /// из futures после первой ошибки паникует, а close-кадр отказа пишется
    /// и после неё.
    pub(crate) struct MapErr<S, E> {
        #[pin]
        sink: S,
        map: fn(E) -> TransportError,
    }
}

impl<S, E> MapErr<S, E> {
    pub(crate) fn new(sink: S, map: fn(E) -> TransportError) -> Self {
        Self { sink, map }
    }
}

impl<S, E, Item> Sink<Item> for MapErr<S, E>
where
    S: Sink<Item, Error = E>,
{
    type Error = TransportError;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.project();
        this.sink.poll_ready(cx).map_err(*this.map)
    }

    fn start_send(self: Pin<&mut Self>, item: Item) -> Result<(), Self::Error> {
        let this = self.project();
        this.sink.start_send(item).map_err(*this.map)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.project();
        this.sink.poll_flush(cx).map_err(*this.map)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = self.project();
        this.sink.poll_close(cx).map_err(*this.map)
    }
}

/// Установленное соединение: кадры в обе стороны, до handshake протокола.
pub struct Conn {
    pub(crate) tx: Tx,
    pub(crate) rx: Rx,
    pub(crate) remote: String,
    /// Байтовый прогресс сокета; у `Conn::new` его отмечает только пир — по кадрам.
    pub(crate) meter: Arc<Meter>,
}

impl Conn {
    pub fn new(
        tx: impl Sink<Outbound, Error = TransportError> + Send + 'static,
        rx: impl Stream<Item = Result<bytes::Bytes, TransportError>> + Send + 'static,
        remote: impl Into<String>,
    ) -> Self {
        Self::metered(tx, rx, remote, Meter::new())
    }

    pub(crate) fn metered(
        tx: impl Sink<Outbound, Error = TransportError> + Send + 'static,
        rx: impl Stream<Item = Result<bytes::Bytes, TransportError>> + Send + 'static,
        remote: impl Into<String>,
        meter: Arc<Meter>,
    ) -> Self {
        Self {
            tx: Box::pin(tx),
            rx: Box::pin(rx),
            remote: remote.into(),
            meter,
        }
    }

    /// Адрес пира для логов.
    pub fn remote(&self) -> &str {
        &self.remote
    }

    /// Сырой кадр мимо пира: handshake и тесты протокола.
    pub async fn send(&mut self, out: Outbound) -> Result<(), TransportError> {
        self.tx.send(out).await
    }

    pub async fn recv(&mut self) -> Option<Result<bytes::Bytes, TransportError>> {
        self.rx.next().await
    }

    /// Пара соединённых концов в памяти: ядро и модуль в одном процессе, тесты.
    pub fn pair(cfg: &Config) -> (Self, Self) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        (
            framed::conn(a, cfg, "memory"),
            framed::conn(b, cfg, "memory"),
        )
    }
}

impl fmt::Debug for Conn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Conn")
            .field("remote", &self.remote)
            .finish_non_exhaustive()
    }
}

/// Подключение модуля: `uds:///path`, `ws://host:port[/v1/modules]`, `wss://…`.
pub async fn connect(target: &Url, cfg: &Config, tls: &ClientTls) -> Result<Conn, TransportError> {
    match target.scheme() {
        "uds" | "unix" => framed::connect(Path::new(target.path()), cfg).await,
        "ws" | "wss" => ws::connect(target, cfg, tls).await,
        other => Err(TransportError::Target(other.to_owned())),
    }
}

/// Слушатель ядра.
pub struct Listener {
    kind: ListenerKind,
    cfg: Config,
    /// Файл унаследованного сокета принадлежит менеджеру служб и на drop не удаляется.
    inherited: bool,
}

enum ListenerKind {
    Uds {
        listener: UnixListener,
        path: PathBuf,
    },
    Ws {
        listener: TcpListener,
    },
    Wss {
        listener: TcpListener,
        acceptor: TlsAcceptor,
    },
}

impl Listener {
    pub async fn bind(listen: &Listen, cfg: &Config) -> Result<Self, TransportError> {
        Self::open(listen, &mut Inherited::default(), cfg).await
    }

    /// Унаследованный сокет с адресом `listen`, иначе свой bind.
    pub async fn open(
        listen: &Listen,
        inherited: &mut Inherited,
        cfg: &Config,
    ) -> Result<Self, TransportError> {
        let (kind, adopted) = match listen {
            Listen::Uds(uds) => {
                let (listener, adopted) = open_uds(&uds.path, inherited)?;
                let path = uds.path.clone();
                (ListenerKind::Uds { listener, path }, adopted)
            }
            Listen::Ws(ws) => {
                let (listener, adopted) = open_tcp(ws.addr, inherited).await?;
                (ListenerKind::Ws { listener }, adopted)
            }
            Listen::Wss(wss) => {
                let (listener, adopted) = open_tcp(wss.addr, inherited).await?;
                let acceptor = TlsAcceptor::from(tls::server_config(&wss.tls).await?);
                (ListenerKind::Wss { listener, acceptor }, adopted)
            }
        };

        Ok(Self {
            kind,
            cfg: cfg.clone(),
            inherited: adopted,
        })
    }

    /// Сокет взят у менеджера служб, а не привязан процессом.
    pub fn inherited(&self) -> bool {
        self.inherited
    }

    /// Фактический адрес TCP-слушателя: порт `0` при bind выбирает система.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        match &self.kind {
            ListenerKind::Uds { .. } => None,
            ListenerKind::Ws { listener } | ListenerKind::Wss { listener, .. } => {
                listener.local_addr().ok()
            }
        }
    }

    /// Принимает сокет; TLS и Upgrade — в `RawConn::upgrade`, чтобы медленный
    /// клиент не держал цикл приёма.
    pub async fn accept(&self) -> Result<RawConn, TransportError> {
        let raw = match &self.kind {
            ListenerKind::Uds { listener, path } => {
                let (stream, _) = listener.accept().await.map_err(TransportError::io)?;
                Raw::Uds(stream, path.display().to_string())
            }
            ListenerKind::Ws { listener } => {
                let (stream, addr) = listener.accept().await.map_err(TransportError::io)?;
                Raw::Ws(stream, addr)
            }
            ListenerKind::Wss { listener, acceptor } => {
                let (stream, addr) = listener.accept().await.map_err(TransportError::io)?;
                Raw::Wss(stream, addr, acceptor.clone())
            }
        };

        Ok(RawConn {
            raw,
            cfg: self.cfg.clone(),
        })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let ListenerKind::Uds { path, .. } = &self.kind
            && !self.inherited
        {
            _ = std::fs::remove_file(path);
        }
    }
}

/// Принятый сокет до TLS и Upgrade.
pub struct RawConn {
    raw: Raw,
    cfg: Config,
}

enum Raw {
    Uds(tokio::net::UnixStream, String),
    Ws(tokio::net::TcpStream, SocketAddr),
    Wss(tokio::net::TcpStream, SocketAddr, TlsAcceptor),
}

impl RawConn {
    /// IP пира TCP-слушателя; у uds его нет.
    pub fn ip(&self) -> Option<IpAddr> {
        match &self.raw {
            Raw::Uds(..) => None,
            Raw::Ws(_, addr) | Raw::Wss(_, addr, _) => Some(addr.ip()),
        }
    }

    /// TLS и WebSocket-handshake; укладываются в `handshake_timeout`.
    pub async fn upgrade(self) -> Result<Conn, TransportError> {
        let Self { raw, cfg } = self;
        let limit: Duration = *cfg.handshake_timeout;

        let upgrade = async {
            match raw {
                Raw::Uds(stream, path) => Ok(framed::conn(stream, &cfg, path)),
                Raw::Ws(stream, addr) => ws::accept(stream, &cfg, addr.to_string()).await,
                Raw::Wss(stream, addr, acceptor) => {
                    let stream = acceptor
                        .accept(stream)
                        .await
                        .map_err(|e| TransportError::Tls(e.to_string()))?;
                    ws::accept(stream, &cfg, addr.to_string()).await
                }
            }
        };

        tokio::time::timeout(limit, upgrade)
            .await
            .map_err(|_| TransportError::Io("transport handshake timed out".to_owned()))?
    }
}

/// Свой bind: права и группу файла сокета задаёт umask процесса.
pub(crate) fn bind_uds(path: &Path) -> Result<UnixListener, TransportError> {
    use std::os::unix::fs::FileTypeExt as _;

    // Сокет прошлого запуска мешает bind; живой сокет другого процесса и
    // не-сокет не трогаем.
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && meta.file_type().is_socket()
    {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(TransportError::Io(format!(
                "{} is served by another process",
                path.display()
            )));
        }
        std::fs::remove_file(path).map_err(TransportError::io)?;
    }

    UnixListener::bind(path).map_err(TransportError::io)
}

/// Унаследованный слушатель по `path`, иначе свой bind; `true` — унаследован.
pub(crate) fn open_uds(
    path: &Path,
    inherited: &mut Inherited,
) -> Result<(UnixListener, bool), TransportError> {
    match inherited.claim(Bound::Unix(path))? {
        Some(fd) => {
            let listener = std::os::unix::net::UnixListener::from(fd);
            listener.set_nonblocking(true).map_err(TransportError::io)?;
            let listener = UnixListener::from_std(listener).map_err(TransportError::io)?;
            Ok((listener, true))
        }
        None => Ok((bind_uds(path)?, false)),
    }
}

/// Унаследованный слушатель по `addr`, иначе свой bind; `true` — унаследован.
pub(crate) async fn open_tcp(
    addr: SocketAddr,
    inherited: &mut Inherited,
) -> Result<(TcpListener, bool), TransportError> {
    match inherited.claim(Bound::Tcp(addr))? {
        Some(fd) => {
            let listener = std::net::TcpListener::from(fd);
            listener.set_nonblocking(true).map_err(TransportError::io)?;
            let listener = TcpListener::from_std(listener).map_err(TransportError::io)?;
            Ok((listener, true))
        }
        None => {
            let listener = TcpListener::bind(addr).await.map_err(TransportError::io)?;
            Ok((listener, false))
        }
    }
}
