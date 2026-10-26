//! Сервисное соединение (`06 §1`): модуль слушает, soul набирает его по
//! `ServiceAddress`. Оба конца — Noise поверх uds или tcp, внутри — кадры uds.

use std::{net::IpAddr, path::PathBuf, time::Duration};

use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream, UnixListener, UnixStream},
    time::Instant,
};

use eva_protocol::types::module::ServiceAddress;

use crate::{
    config::{Config, ServiceListen},
    transport::{
        Conn, TransportError,
        activation::Inherited,
        framed,
        meter::{Meter, Metered},
        noise::{self, Psk},
        open_tcp, open_uds,
    },
};

/// Сторона soul: connect, затем Noise initiator; всё — за `handshake_timeout`.
pub async fn dial(
    address: &ServiceAddress,
    psk: &Psk,
    cfg: &Config,
) -> Result<Conn, TransportError> {
    dial_by(address, psk, cfg, Instant::now() + *cfg.handshake_timeout).await
}

/// `deadline` — общий с `hello` у `Server::dial`: вся попытка считается от connect.
pub(crate) async fn dial_by(
    address: &ServiceAddress,
    psk: &Psk,
    cfg: &Config,
    deadline: Instant,
) -> Result<Conn, TransportError> {
    let remote = address.to_string();
    let dial = async {
        match address {
            ServiceAddress::Uds(path) => {
                let stream = UnixStream::connect(path)
                    .await
                    .map_err(TransportError::io)?;
                secure(stream, psk, cfg, remote.clone(), Role::Initiator).await
            }
            ServiceAddress::Tcp { host, port } => {
                let stream = TcpStream::connect((host.as_str(), *port))
                    .await
                    .map_err(TransportError::io)?;
                stream.set_nodelay(true).map_err(TransportError::io)?;
                secure(stream, psk, cfg, remote.clone(), Role::Initiator).await
            }
        }
    };
    tokio::time::timeout_at(deadline, dial)
        .await
        .map_err(|_| TransportError::Io(format!("{remote}: connect and noise timed out")))?
}

#[derive(Clone, Copy)]
enum Role {
    Initiator,
    Responder,
}

/// Noise поверх сокета и кадры поверх Noise; живость — по байтам сокета.
async fn secure<S>(
    io: S,
    psk: &Psk,
    cfg: &Config,
    remote: String,
    role: Role,
) -> Result<Conn, TransportError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let meter = Meter::new();
    let io = Metered::new(io, meter.clone());
    let stream = match role {
        Role::Initiator => noise::initiate(io, psk).await,
        Role::Responder => noise::respond(io, psk).await,
    }
    .map_err(|e| TransportError::Noise(e.to_string()))?;
    Ok(framed::over(stream, cfg, remote, meter))
}

/// Слушатель сервисного модуля; принимает только держателя его PSK.
pub struct ServiceListener {
    kind: ListenerKind,
    psk: Psk,
    cfg: Config,
}

enum ListenerKind {
    Uds {
        listener: UnixListener,
        /// Свой сокет, удаляемый на drop; у унаследованного — `None`.
        owned: Option<PathBuf>,
    },
    Tcp {
        listener: TcpListener,
    },
}

impl ServiceListener {
    pub async fn bind(
        listen: &ServiceListen,
        psk: Psk,
        cfg: &Config,
    ) -> Result<Self, TransportError> {
        Self::open(listen, &mut Inherited::default(), psk, cfg).await
    }

    /// Унаследованный сокет с адресом `listen` (socket activation), иначе
    /// свой bind. Файл унаследованного сокета принадлежит менеджеру служб.
    pub async fn open(
        listen: &ServiceListen,
        inherited: &mut Inherited,
        psk: Psk,
        cfg: &Config,
    ) -> Result<Self, TransportError> {
        let kind = match listen {
            ServiceListen::Uds(uds) => {
                let (listener, adopted) = open_uds(&uds.path, inherited)?;
                let owned = (!adopted).then(|| uds.path.clone());
                ListenerKind::Uds { listener, owned }
            }
            ServiceListen::Tcp(tcp) => ListenerKind::Tcp {
                listener: open_tcp(tcp.addr, inherited).await?.0,
            },
        };
        Ok(Self {
            kind,
            psk,
            cfg: cfg.clone(),
        })
    }

    /// Адрес, по которому звонит soul: порт `0` при bind выбирает система.
    pub fn local_addr(&self) -> Option<ServiceAddress> {
        match &self.kind {
            ListenerKind::Uds { listener, .. } => {
                let addr = listener.local_addr().ok()?;
                Some(ServiceAddress::Uds(addr.as_pathname()?.to_path_buf()))
            }
            ListenerKind::Tcp { listener } => {
                let addr = listener.local_addr().ok()?;
                Some(ServiceAddress::Tcp {
                    host: addr.ip().to_string(),
                    port: addr.port(),
                })
            }
        }
    }

    /// Принимает сокет; Noise — в `Incoming::secure`, чтобы медленный пир
    /// не держал цикл приёма.
    pub async fn accept(&self) -> Result<Incoming, TransportError> {
        let (stream, remote, ip) = match &self.kind {
            ListenerKind::Uds { listener, .. } => {
                let (stream, _) = listener.accept().await.map_err(TransportError::io)?;
                let remote = self
                    .local_addr()
                    .map_or_else(|| String::from("uds"), |addr| addr.to_string());
                (Stream::Uds(stream), remote, None)
            }
            ListenerKind::Tcp { listener } => {
                let (stream, addr) = listener.accept().await.map_err(TransportError::io)?;
                stream.set_nodelay(true).map_err(TransportError::io)?;
                (Stream::Tcp(stream), addr.to_string(), Some(addr.ip()))
            }
        };
        Ok(Incoming {
            stream,
            remote,
            ip,
            psk: self.psk.clone(),
            cfg: self.cfg.clone(),
        })
    }
}

impl Drop for ServiceListener {
    fn drop(&mut self) {
        if let ListenerKind::Uds {
            owned: Some(path), ..
        } = &self.kind
        {
            _ = std::fs::remove_file(path);
        }
    }
}

/// Принятый сокет до Noise.
pub struct Incoming {
    stream: Stream,
    remote: String,
    ip: Option<IpAddr>,
    psk: Psk,
    cfg: Config,
}

enum Stream {
    Uds(UnixStream),
    Tcp(TcpStream),
}

impl Incoming {
    /// IP пира tcp-слушателя; у uds его нет.
    pub fn ip(&self) -> Option<IpAddr> {
        self.ip
    }

    /// Адрес пира для логов: у tcp — `ip:port`, у uds — путь сокета.
    pub fn remote(&self) -> &str {
        &self.remote
    }

    /// Noise responder за `handshake_timeout`. Чужой PSK — ошибка и сокет
    /// закрыт без ответа: soul видит обрыв.
    pub async fn secure(self) -> Result<Conn, TransportError> {
        let Self {
            stream,
            remote,
            psk,
            cfg,
            ..
        } = self;
        let limit: Duration = *cfg.handshake_timeout;
        let secure = async {
            match stream {
                Stream::Uds(stream) => secure(stream, &psk, &cfg, remote, Role::Responder).await,
                Stream::Tcp(stream) => secure(stream, &psk, &cfg, remote, Role::Responder).await,
            }
        };
        tokio::time::timeout(limit, secure)
            .await
            .map_err(|_| TransportError::Noise("handshake timed out".to_owned()))?
    }
}
