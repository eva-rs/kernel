//! Сервер на uds/ws с тестовым soul, набираемый им сервис на uds/tcp и сырой
//! uds-клиент для проверок протокола.

use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use futures::{SinkExt as _, StreamExt as _};
use tokio::{
    io::AsyncWriteExt as _,
    net::UnixStream,
    sync::{mpsc, oneshot},
};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use eva::{time::HumanDuration, url::Url};
use eva_protocol::{
    types::{ModuleId, SessionId},
    wire::{
        Capabilities, Frame, RpcError,
        frame::{Payload, Request},
        handshake::{Hello, ProtoVersion, Resume, Welcome},
        method,
    },
};
use eva_rpc::{
    Accepted, ClientOptions, Config, Listener, ModuleHandler, NoHandler, Psk, RpcClient, Server,
    ServiceListen, ServiceListener, codec,
    config::{Listen, TcpListen, UdsListen, WsListen},
};

use crate::soul::{Auth, SERVICE, Soul};

/// Слушатель soul, куда звонит клиент.
#[derive(Debug, Clone, Copy)]
pub enum Via {
    Uds,
    Ws,
}

/// Слушатель сервиса, куда дозванивается soul.
#[derive(Debug, Clone, Copy)]
pub enum Dial {
    Uds,
    Tcp,
}

/// Секрет сервиса `SERVICE` у обеих сторон.
pub const KEY: &str = "0123456789abcdef0123456789abcdef";

pub fn ms(ms: u64) -> HumanDuration {
    HumanDuration(Duration::from_millis(ms))
}

/// Короткие тайминги: тесты не ждут минутами.
pub fn config() -> Config {
    Config {
        handshake_timeout: ms(2_000),
        request_timeout: ms(5_000),
        ..Config::default()
    }
}

pub struct Harness {
    pub soul: Soul,
    pub server: Server<Soul, Auth>,
    pub target: Url,
    pub socket: Option<PathBuf>,
    pub config: Config,
    pub sessions: mpsc::UnboundedReceiver<Accepted>,
    _stop: oneshot::Sender<()>,
    dir: tempfile::TempDir,
}

impl Harness {
    pub async fn start(via: Via) -> Self {
        Self::with(via, config()).await
    }

    pub async fn with(via: Via, config: Config) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("modules.sock");
        let listen = match via {
            Via::Uds => Listen::Uds(UdsListen {
                path: socket.clone(),
            }),
            Via::Ws => Listen::Ws(WsListen {
                addr: "127.0.0.1:0".parse().unwrap(),
            }),
        };
        let listener = Listener::bind(&listen, &config).await.unwrap();
        let (target, socket) = match via {
            Via::Uds => (
                Url::parse(&format!("uds://{}", socket.display())).unwrap(),
                Some(socket),
            ),
            Via::Ws => (
                Url::parse(&format!(
                    "ws://{}/v1/modules",
                    listener.local_addr().unwrap()
                ))
                .unwrap(),
                None,
            ),
        };

        let soul = Soul::default();
        let server = Server::new(soul.clone(), Auth::default(), config.clone());
        let (sessions_tx, sessions) = mpsc::unbounded_channel();
        let (stop, stopped) = oneshot::channel::<()>();
        let serving = server.clone();
        tokio::spawn(async move {
            let server = serving;
            let shutdown = async {
                _ = stopped.await;
            };
            server
                .serve(listener, shutdown, move |accepted| {
                    _ = sessions_tx.send(accepted);
                })
                .await;
        });

        Self {
            soul,
            server,
            target,
            socket,
            config,
            sessions,
            _stop: stop,
            dir,
        }
    }

    /// Слушатель сервиса с PSK из `key`; у tcp порт выбирает система.
    pub async fn listen(&self, dial: Dial, key: &str) -> ServiceListener {
        static SOCKETS: AtomicUsize = AtomicUsize::new(0);
        let listen = match dial {
            Dial::Uds => ServiceListen::Uds(UdsListen {
                path: self.dir.path().join(format!(
                    "service-{}.sock",
                    SOCKETS.fetch_add(1, Ordering::Relaxed)
                )),
            }),
            Dial::Tcp => ServiceListen::Tcp(TcpListen {
                addr: "127.0.0.1:0".parse().unwrap(),
            }),
        };
        ServiceListener::bind(&listen, Psk::derive(key), &self.config)
            .await
            .unwrap()
    }

    /// Soul набирает сервис `SERVICE`, тот отвечает `hello` с `handler`.
    pub async fn dial_with<H: ModuleHandler>(
        &self,
        dial: Dial,
        hello: Hello,
        handler: H,
    ) -> (RpcClient, Accepted) {
        let listener = self.listen(dial, KEY).await;
        let address = listener.local_addr().unwrap();
        let options = self.options();
        let module = tokio::spawn(async move {
            let conn = listener.accept().await.unwrap().secure().await.unwrap();
            RpcClient::answer(conn, hello, options, handler).await
        });
        let accepted = self
            .server
            .dial(&address, &Psk::derive(KEY), &ModuleId::from(SERVICE))
            .await
            .unwrap();
        let (client, welcome) = module.await.unwrap().unwrap();
        assert_eq!(accepted.welcome.session_id, welcome.session_id);
        (client, accepted)
    }

    pub fn options(&self) -> ClientOptions {
        ClientOptions {
            config: self.config.clone(),
            ..ClientOptions::default()
        }
    }

    pub async fn connect(&mut self, token: &str) -> (RpcClient, Accepted) {
        self.connect_with(hello(token), NoHandler).await
    }

    pub async fn connect_with<H: ModuleHandler>(
        &mut self,
        hello: Hello,
        handler: H,
    ) -> (RpcClient, Accepted) {
        let (client, welcome) =
            RpcClient::connect_with(&self.target, hello, self.options(), handler)
                .await
                .unwrap();
        let accepted = self.accepted().await;
        assert_eq!(accepted.welcome.session_id, welcome.session_id);
        (client, accepted)
    }

    pub async fn accepted(&mut self) -> Accepted {
        tokio::time::timeout(Duration::from_secs(5), self.sessions.recv())
            .await
            .expect("no session accepted")
            .expect("server stopped")
    }

    pub async fn raw(&self) -> Raw {
        let path = self.socket.as_ref().expect("raw frames need uds");
        Raw::connect(path, &self.config).await
    }
}

pub fn hello(token: &str) -> Hello {
    Hello {
        token: Some(token.into()),
        ..service_hello()
    }
}

/// `hello` набранного сервиса: без токена.
pub fn service_hello() -> Hello {
    Hello {
        version: "0.0.0".to_owned(),
        proto_version: ProtoVersion::CURRENT,
        token: None,
        capabilities: Capabilities::default(),
        resume: None,
    }
}

pub fn resume(mut hello: Hello, session: &SessionId) -> Hello {
    hello.resume = Some(Resume {
        session_id: session.clone(),
    });
    hello
}

/// uds-клиент без пира: кадры руками.
pub struct Raw {
    framed: Framed<UnixStream, LengthDelimitedCodec>,
}

impl Raw {
    pub async fn connect(path: &PathBuf, config: &Config) -> Self {
        let stream = UnixStream::connect(path).await.unwrap();
        let codec = LengthDelimitedCodec::builder()
            .big_endian()
            .length_field_type::<u32>()
            .max_frame_length(config.max_frame())
            .new_codec();
        Self {
            framed: Framed::new(stream, codec),
        }
    }

    pub async fn send(&mut self, frame: &Frame) {
        let raw = codec::encode_frame(frame).unwrap();
        self.framed.send(raw).await.unwrap();
    }

    /// Байты как есть, мимо кодека.
    pub async fn send_bytes(&mut self, bytes: &[u8]) {
        self.framed.get_mut().write_all(bytes).await.unwrap();
    }

    pub async fn request(&mut self, id: u64, method: &str, params: Payload) {
        self.send(&Frame::Request(Request {
            id,
            method: method.to_owned(),
            params,
        }))
        .await;
    }

    /// Следующий кадр, кроме `ping`; `None` — сокет закрыт.
    pub async fn recv(&mut self) -> Option<Frame> {
        loop {
            let raw = tokio::time::timeout(Duration::from_secs(5), self.framed.next())
                .await
                .expect("no frame in time")?;
            let Ok(raw) = raw else {
                return None;
            };
            match codec::decode_frame(&raw).unwrap() {
                Frame::Notify(notify) if notify.method == method::PING => {}
                frame => return Some(frame),
            }
        }
    }

    pub async fn hello(&mut self, hello: &Hello) -> Result<Welcome, RpcError> {
        self.request(0, method::HELLO, codec::encode(hello).unwrap())
            .await;
        match self.recv().await {
            Some(Frame::Response(response)) => {
                assert_eq!(response.id, 0);
                response
                    .into_result()
                    .map(|welcome| codec::decode(&welcome).unwrap())
            }
            other => panic!("expected welcome, got {other:?}"),
        }
    }

    /// Ответ на `id`, пропуская прочее.
    pub async fn response(&mut self, id: u64) -> Result<Payload, RpcError> {
        loop {
            match self.recv().await {
                Some(Frame::Response(response)) if response.id == id => {
                    return response.into_result();
                }
                Some(_) => {}
                None => panic!("connection closed before response {id}"),
            }
        }
    }

    /// Сокет закрыт сервером: дальше только EOF.
    pub async fn closed(&mut self) -> bool {
        loop {
            match self.recv().await {
                None => return true,
                Some(Frame::Response(_) | Frame::StreamEnd(_)) => {}
                Some(Frame::Notify(notify)) if notify.method == method::CLOSE => {}
                Some(other) => panic!("unexpected frame {other:?}"),
            }
        }
    }
}
