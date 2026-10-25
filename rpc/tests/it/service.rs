//! Сервисное соединение: ядро набирает модуль по uds и tcp под Noise, `hello`
//! шлёт модуль, `welcome` — ядро.

use std::time::Duration;

use futures::stream;

use eva::bytesize::ByteSize;

use eva_protocol::{
    requests::{ByteStream, blobs::put, surfaces::create},
    types::{ModuleId, blob::Kind as BlobKind, module::ModuleKind},
    wire::ErrorCode,
};
use eva_rpc::{
    ClientOptions, Conn, ConnectError, DialError, NoHandler, Psk, RpcClient, TransportError,
    config::Config, transport::noise::MAX_PLAINTEXT,
};

use crate::{
    harness::{Dial, Harness, KEY, Via, config, hello, ms, service_hello},
    kernel::{CLIENT_TOKEN, REMOVED, SERVICE, chat_id},
};

const BOTH: [Dial; 2] = [Dial::Uds, Dial::Tcp];

fn short() -> Config {
    Config {
        handshake_timeout: ms(300),
        ..config()
    }
}

fn service() -> ModuleId {
    ModuleId::from(SERVICE)
}

/// Кадры в обе стороны; кадр больше сообщения Noise режется на несколько.
#[tokio::test(flavor = "multi_thread")]
async fn frames_over_noise() {
    for dial in BOTH {
        let h = Harness::start(Via::Uds).await;
        let (module, accepted) = h.dial_with(dial, service_hello(), NoHandler).await;
        assert_eq!(accepted.welcome.kind, ModuleKind::Service, "{dial:?}");
        assert_eq!(accepted.welcome.module.as_str(), SERVICE);
        assert_eq!(module.caller(), &accepted.caller);
        assert!(!module.caller().is_owner());
        assert_eq!(accepted.hello.token, None);

        let mut session = module.session();
        let create::Ok { surface } = session
            .surfaces()
            .create("tg".into(), "tg".to_owned(), None)
            .await
            .unwrap();
        assert_eq!(surface.id.as_str(), "tg");

        let big = (0..3 * MAX_PLAINTEXT + 7)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>();
        let body: ByteStream = Box::pin(stream::iter([Ok(big.clone().into())]));
        let ok = session
            .blobs()
            .put(
                put::Meta {
                    chat: chat_id(1),
                    name: "big.bin".to_owned(),
                    media_type: "application/octet-stream".to_owned(),
                    kind: BlobKind::File,
                    size: big.len() as u64,
                    hash: None,
                },
                body,
            )
            .await
            .unwrap();
        assert_eq!(ok.size, big.len() as u64);
        assert_eq!(*h.kernel.state.uploads.lock().unwrap(), vec![big]);
    }
}

/// Чужой PSK: модуль рвёт соединение на первом сообщении, ядро видит обрыв
/// сразу, а не по таймауту; до `hello` дело не доходит.
#[tokio::test(flavor = "multi_thread")]
async fn wrong_key_is_refused_on_both_sides() {
    for dial in BOTH {
        let h = Harness::start(Via::Uds).await;
        let listener = h.listen(dial, "another key of the module, 32+ chars").await;
        let address = listener.local_addr().unwrap();
        let module = tokio::spawn(async move { listener.accept().await.unwrap().secure().await });

        let started = tokio::time::Instant::now();
        let kernel = h
            .server
            .dial(&address, &Psk::derive(KEY), &service())
            .await
            .unwrap_err();
        assert!(
            started.elapsed() < *h.config.handshake_timeout,
            "{dial:?}: waited for the timeout"
        );
        match &kernel {
            DialError::Transport(TransportError::Noise(message)) => {
                assert!(message.contains("wrong key"), "{dial:?}: {message}");
            }
            other => panic!("{dial:?}: expected a noise failure, got {other:?}"),
        }

        match module.await.unwrap() {
            Err(TransportError::Noise(message)) => {
                assert!(message.contains("wrong key"), "{dial:?}: {message}");
            }
            other => panic!("{dial:?}: the module must refuse, got {other:?}"),
        }
    }
}

/// Набранный модуль токена не шлёт: SDK не отправит его вовсе, а ядро
/// отвергнет `invalid_params`.
#[tokio::test(flavor = "multi_thread")]
async fn dialed_hello_carries_no_token() {
    let (_kernel, module) = Conn::pair(&config());
    let local = RpcClient::answer(
        module,
        hello(CLIENT_TOKEN),
        ClientOptions::default(),
        NoHandler,
    )
    .await;
    assert!(matches!(local, Err(ConnectError::Config(_))), "{local:?}");

    let h = Harness::start(Via::Uds).await;
    let listener = h.listen(Dial::Uds, KEY).await;
    let address = listener.local_addr().unwrap();
    let options = h.options();
    let module = tokio::spawn(async move {
        let conn = listener.accept().await.unwrap().secure().await.unwrap();
        // Клиентский handshake поверх набранного соединения: токен уйдёт.
        RpcClient::handshake(conn, hello(CLIENT_TOKEN), options, NoHandler).await
    });
    let kernel = h
        .server
        .dial(&address, &Psk::derive(KEY), &service())
        .await
        .unwrap_err();
    let DialError::Handshake(error) = kernel else {
        panic!("expected a handshake refusal, got {kernel:?}");
    };
    assert_eq!(error.code, ErrorCode::InvalidParams);
    match module.await.unwrap() {
        Err(ConnectError::Rejected(e)) => assert_eq!(e.code, ErrorCode::InvalidParams),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

/// PSK уже доказан: `hello` сервиса держит лишь `max_frame`, не `max_hello`.
#[tokio::test(flavor = "multi_thread")]
async fn dialed_hello_is_bounded_by_max_frame_only() {
    let h = Harness::with(
        Via::Uds,
        Config {
            max_hello: ByteSize::kib(64),
            ..config()
        },
    )
    .await;
    let mut big = service_hello();
    big.version = "x".repeat(256 << 10);
    let (_module, accepted) = h.dial_with(Dial::Uds, big, NoHandler).await;
    assert_eq!(accepted.hello.version.len(), 256 << 10);
}

/// Отказ `authenticate` набранному модулю уходит ему ответом на `hello`.
#[tokio::test(flavor = "multi_thread")]
async fn refusal_reaches_the_dialed_module() {
    let h = Harness::start(Via::Uds).await;
    let listener = h.listen(Dial::Tcp, KEY).await;
    let address = listener.local_addr().unwrap();
    let options = h.options();
    let module = tokio::spawn(async move {
        let conn = listener.accept().await.unwrap().secure().await.unwrap();
        RpcClient::answer(conn, service_hello(), options, NoHandler).await
    });

    let kernel = h
        .server
        .dial(&address, &Psk::derive(KEY), &ModuleId::from(REMOVED))
        .await
        .unwrap_err();
    assert!(
        matches!(&kernel, DialError::Handshake(e) if e.code == ErrorCode::Unauthorized),
        "{kernel:?}"
    );
    match module.await.unwrap() {
        Err(ConnectError::Rejected(e)) => {
            assert_eq!(e.code, ErrorCode::Unauthorized);
            assert_eq!(e.message, "module removed");
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
}

/// Connect, Noise и `hello` укладываются в один `handshake_timeout` от connect.
#[tokio::test(flavor = "multi_thread")]
async fn handshake_timeout_bounds_the_attempt() {
    let h = Harness::with(Via::Uds, short()).await;
    let limit = *h.config.handshake_timeout;

    // Сокет принят, но Noise молчит.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mute.sock");
    let mute = tokio::net::UnixListener::bind(&path).unwrap();
    let holder = tokio::spawn(async move { mute.accept().await.map(|(stream, _)| stream) });
    let started = tokio::time::Instant::now();
    let error = h
        .server
        .dial(
            &format!("uds://{}", path.display()).parse().unwrap(),
            &Psk::derive(KEY),
            &service(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&error, DialError::Transport(TransportError::Io(m)) if m.contains("timed out")),
        "{error:?}"
    );
    assert!(started.elapsed() < limit * 3, "{:?}", started.elapsed());
    drop(holder);

    // Noise сошёлся, `hello` не пришёл.
    let listener = h.listen(Dial::Tcp, KEY).await;
    let address = listener.local_addr().unwrap();
    let module = tokio::spawn(async move {
        let conn = listener.accept().await.unwrap().secure().await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
        drop(conn);
    });
    let started = tokio::time::Instant::now();
    let error = h
        .server
        .dial(&address, &Psk::derive(KEY), &service())
        .await
        .unwrap_err();
    assert!(
        matches!(&error, DialError::Handshake(e) if e.code == ErrorCode::Timeout),
        "{error:?}"
    );
    assert!(started.elapsed() < limit * 3, "{:?}", started.elapsed());
    module.abort();
}

/// Бюджет один на всё: поздний Noise съедает время, оставленное `hello`, а
/// не открывает ему новый `handshake_timeout`.
#[tokio::test(flavor = "multi_thread")]
async fn late_noise_leaves_hello_the_rest_of_one_budget() {
    let limit = Duration::from_secs(1);
    let late = limit * 7 / 10;
    let h = Harness::with(
        Via::Uds,
        Config {
            handshake_timeout: eva::time::HumanDuration(limit),
            ..config()
        },
    )
    .await;
    let listener = h.listen(Dial::Uds, KEY).await;
    let address = listener.local_addr().unwrap();
    let module = tokio::spawn(async move {
        let incoming = listener.accept().await.unwrap();
        tokio::time::sleep(late).await;
        let conn = incoming.secure().await.unwrap();
        tokio::time::sleep(limit * 5).await;
        drop(conn);
    });

    let started = tokio::time::Instant::now();
    let error = h
        .server
        .dial(&address, &Psk::derive(KEY), &service())
        .await
        .unwrap_err();
    let took = started.elapsed();
    assert!(
        matches!(&error, DialError::Handshake(e) if e.code == ErrorCode::Timeout),
        "{error:?}"
    );
    assert!(took >= late, "noise finished early: {took:?}");
    assert!(
        took < limit + Duration::from_millis(250),
        "hello got a budget of its own: {took:?}"
    );
    module.abort();
}

/// Модуль не держит сокет, который молчит вместо Noise, дольше `handshake_timeout`.
#[tokio::test(flavor = "multi_thread")]
async fn module_side_noise_times_out() {
    let h = Harness::with(Via::Uds, short()).await;
    let listener = h.listen(Dial::Uds, KEY).await;
    let path = listener.local_addr().unwrap().path().unwrap().to_owned();
    let _mute = tokio::net::UnixStream::connect(&path).await.unwrap();

    let started = tokio::time::Instant::now();
    let error = listener.accept().await.unwrap().secure().await.unwrap_err();
    assert!(matches!(error, TransportError::Noise(_)), "{error:?}");
    assert!(started.elapsed() < *h.config.handshake_timeout * 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_service_is_a_transport_error() {
    let h = Harness::start(Via::Uds).await;
    let dir = tempfile::tempdir().unwrap();
    let missing = format!("uds://{}", dir.path().join("none.sock").display());

    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);

    for address in [missing, format!("tcp://127.0.0.1:{port}")] {
        let error = h
            .server
            .dial(&address.parse().unwrap(), &Psk::derive(KEY), &service())
            .await
            .unwrap_err();
        assert!(
            matches!(error, DialError::Transport(TransportError::Io(_))),
            "{address}: {error:?}"
        );
    }
}
