//! Сервис: модуль слушает, поддельное ядро набирает его по uds и tcp под
//! Noise; новое соединение ядра вытесняет прежнее, `run` переживает потерю ядра.

use std::{
    sync::{Arc, atomic::AtomicUsize, atomic::Ordering},
    time::Duration,
};

use futures::StreamExt as _;
use tokio::{io::AsyncReadExt as _, sync::oneshot};

use eva::time::HumanDuration;
use eva_core::prelude::*;
use eva_protocol::{
    errors::{Aux, Generic},
    requests::surfaces::create,
    types::module::ServiceAddress,
    wire::{ErrorCode, RpcError, Welcome, tools::ToolItem},
};
use eva_rpc::{
    Config, Psk, Service, TransportError,
    config::{TcpListen, UdsListen},
    gate::MAX_PENDING_PER_NET,
    transport,
};
use eva_sdk::{
    Caller, ConnectError, Listening, Module, SecretString, ServiceConfig, ServiceListen,
    ToolContent,
};

use crate::{
    fake::{self, Surfaces},
    reconnect::{
        Silent, get, id_of, surface_x,
        tools::{Counters, call, slow, wait},
        update,
    },
};

const KEY: &str = "a key of the x-bridge service, 32+ chars";

#[derive(Debug, Clone, Copy)]
enum Via {
    Uds,
    Tcp,
}

const BOTH: [Via; 2] = [Via::Uds, Via::Tcp];

fn config(dir: &tempfile::TempDir, via: Via) -> ServiceConfig {
    ServiceConfig {
        listen: match via {
            Via::Uds => ServiceListen::Uds(UdsListen {
                path: dir.path().join("module.sock"),
            }),
            Via::Tcp => ServiceListen::Tcp(TcpListen {
                addr: "127.0.0.1:0".parse().unwrap(),
            }),
        },
        key: SecretString::Plaintext(KEY.to_owned()),
        kernel_wait: HumanDuration(Duration::from_secs(5)),
        transport: Config::default(),
    }
}

/// Модуль слушает; адрес — тот, что ядро записало бы в реестр.
async fn listen(module: Module, cfg: &ServiceConfig) -> (Listening, ServiceAddress) {
    let listening = module.listen(cfg).await.unwrap();
    let address = listening
        .address()
        .expect("a bound listener has an address");
    (listening, address)
}

/// Поддельное ядро набирает модуль: Noise с ключом записи, `welcome` на `hello`.
async fn dial(address: &ServiceAddress, welcome: Welcome, service: impl Service) -> fake::Session {
    let conn = transport::dial(address, &Psk::derive(KEY), &Config::default())
        .await
        .unwrap();
    fake::accept(conn, welcome, service).await
}

async fn wait_session(listening: &Listening, session: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while listening
            .welcome()
            .is_none_or(|w| w.session_id.as_str() != session)
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the module never switched to {session}"));
}

#[tokio::test]
async fn the_kernel_dials_the_module() {
    for via in BOTH {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(&dir, via);
        let (listening, address) = listen(Module::new().ask(true), &cfg).await;
        if let Via::Uds = via {
            assert_eq!(address, ServiceAddress::Uds(dir.path().join("module.sock")));
        }
        assert!(listening.module().is_none(), "{via:?}");

        let waiting = tokio::spawn({
            let listening = listening.clone();
            async move { listening.kernel().await }
        });
        let surfaces = Surfaces::default();
        let session = dial(&address, fake::welcome("s1", &[]), surfaces.clone()).await;
        assert!(
            session.hello.token.is_none(),
            "{via:?}: a service sends no token"
        );
        assert!(session.hello.resume.is_none());
        assert!(session.hello.capabilities.ask);

        let mut kernel = waiting.await.unwrap().expect("the first welcome");
        assert_eq!(listening.module().unwrap().as_str(), fake::MODULE);
        assert!(matches!(kernel.caller(), Caller::Service { .. }));
        let create::Ok { surface } = kernel
            .surfaces()
            .create()
            .call(create::Args {
                id: "x".into(),
                name: "X".into(),
                rules: None,
            })
            .await
            .unwrap();
        assert_eq!(surface.id.as_str(), "x");
        assert_eq!(surfaces.calls(), ["surfaces.create"], "{via:?}");
    }
}

/// Второе соединение ядра после `welcome` закрывает первое `superseded`;
/// `Kernel`, взятый до него, работает через новое; `run` не завершается.
#[tokio::test]
async fn a_new_kernel_connection_supersedes_the_old_one() {
    let dir = tempfile::tempdir().unwrap();
    let (listening, address) = listen(Module::new(), &config(&dir, Via::Uds)).await;
    let (stop, stopped) = oneshot::channel::<()>();
    let running = tokio::spawn(listening.clone().run(async {
        _ = stopped.await;
    }));

    let first = dial(&address, fake::welcome("s1", &[]), Surfaces::default()).await;
    let kernel = listening.kernel().await.unwrap();

    let second_kernel = surface_x("X2");
    let second = dial(&address, fake::resumed("s2"), second_kernel.clone()).await;
    let resume = second
        .hello
        .resume
        .as_ref()
        .expect("resume the last session");
    assert_eq!(resume.session_id.as_str(), "s1");
    assert!(second.hello_id > first.hello_id, "ids continue");

    let reason = tokio::time::timeout(Duration::from_secs(5), first.peer.closed())
        .await
        .expect("the module closes the old connection");
    assert_eq!(reason.code, ErrorCode::Conflict);
    assert_eq!(reason.message, "superseded");
    wait_session(&listening, "s2").await;

    let read = get(kernel).await.expect("served by the new connection");
    assert_eq!(read.surface.name, "X2");
    assert_eq!(second_kernel.calls(), ["surfaces.get"]);
    assert!(!running.is_finished(), "supersede does not end run");

    stop.send(()).unwrap();
    running.await.unwrap().unwrap();
}

/// Рестарт ядра (`resumed = false`): мутация в полёте — `Aux::Unavailable`
/// без повтора, чтение повторяется на новом соединении.
#[tokio::test]
async fn a_kernel_restart_cuts_a_mutation_and_resends_a_read() {
    let dir = tempfile::tempdir().unwrap();
    let (listening, address) = listen(Module::new(), &config(&dir, Via::Tcp)).await;
    let silent = Silent::default();
    let _first = dial(&address, fake::welcome("s1", &[]), silent.clone()).await;
    let kernel = listening.kernel().await.unwrap();

    let read = tokio::spawn(get(kernel.clone()));
    let write = tokio::spawn(update(kernel.clone()));
    silent.wait_calls(2).await;

    let restarted = surface_x("X");
    let second = dial(&address, fake::welcome("s2", &[]), restarted.clone()).await;
    assert!(second.hello.resume.is_some(), "the module asks to resume");

    let write = tokio::time::timeout(Duration::from_secs(5), write)
        .await
        .expect("the cut write ends")
        .unwrap();
    assert!(
        matches!(&write, Err(Generic::Aux(Aux::Unavailable(m))) if m.contains("outcome unknown")),
        "{write:?}"
    );
    let read = tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .expect("the read is resent")
        .unwrap();
    assert_eq!(read.expect("reads survive any switch").surface.name, "X");
    let requests = restarted.requests.lock().unwrap().clone();
    assert!(
        id_of(&requests, "surfaces.update").is_empty(),
        "{requests:?}"
    );
}

/// Ядро пропало: `run` живёт, вызов ждёт следующего дозвона и уходит в него.
#[tokio::test]
async fn run_outlives_the_kernel_and_calls_wait_for_the_next_dial() {
    let dir = tempfile::tempdir().unwrap();
    let (listening, address) = listen(Module::new(), &config(&dir, Via::Uds)).await;
    let (stop, stopped) = oneshot::channel::<()>();
    let running = tokio::spawn(listening.clone().run(async {
        _ = stopped.await;
    }));

    let first = dial(&address, fake::welcome("s1", &[]), Silent::default()).await;
    let kernel = listening.kernel().await.unwrap();
    let old = listening.client().unwrap();
    first
        .peer
        .close(RpcError::new(ErrorCode::Unavailable, "kernel restarts"));
    tokio::time::timeout(Duration::from_secs(5), old.closed())
        .await
        .expect("the module sees the close");
    drop(old);

    let write = tokio::spawn(update(kernel));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!write.is_finished(), "the call waits for the kernel");
    assert!(!running.is_finished(), "run outlives the kernel");

    let restarted = surface_x("X");
    let second = dial(&address, fake::welcome("s2", &[]), restarted.clone()).await;
    tokio::time::timeout(Duration::from_secs(5), write)
        .await
        .expect("the call proceeds on the new connection")
        .unwrap()
        .expect("served by the new connection");
    assert_eq!(restarted.calls(), ["surfaces.update"]);

    stop.send(()).unwrap();
    running.await.unwrap().unwrap();
    let reason = tokio::time::timeout(Duration::from_secs(5), second.peer.closed())
        .await
        .expect("shutdown closes the connection");
    assert_eq!(reason.code, ErrorCode::Unavailable);
}

/// Обрыв посреди `tools.call`: ядро перезванивает с `hello.resume`, повтор
/// того же `call_id` исполняется один раз (`06 §12`).
#[tokio::test]
async fn a_tool_call_survives_a_redial_and_runs_once() {
    let dir = tempfile::tempdir().unwrap();
    let counters = Arc::new(Counters {
        started: AtomicUsize::new(0),
        finished: AtomicUsize::new(0),
        gate: tokio::sync::Notify::new(),
    });
    let (_listening, address) = listen(slow(counters.clone()), &config(&dir, Via::Uds)).await;

    let first = dial(&address, fake::welcome("s1", &[]), Surfaces::default()).await;
    let module = first.module();
    let pending = tokio::spawn(async move {
        let items: Vec<_> = module.call_tool(&call()).await?.collect().await;
        Ok::<_, RpcError>(items)
    });
    wait("the tool to start", || {
        counters.started.load(Ordering::SeqCst) == 1
    })
    .await;
    first
        .peer
        .close(RpcError::new(ErrorCode::Unavailable, "socket lost"));
    _ = pending.await;

    let second = dial(&address, fake::resumed("s2"), Surfaces::default()).await;
    assert!(second.hello.resume.is_some());
    let module = second.module();
    let retry = tokio::spawn(async move {
        let items: Vec<_> = module.call_tool(&call()).await.unwrap().collect().await;
        items
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    counters.gate.notify_waiters();
    let items = retry.await.unwrap();
    match items.last() {
        Some(Ok(ToolItem::Result(result))) => {
            assert_eq!(result.content, vec![ToolContent::text("done")]);
        }
        other => panic!("expected the result, got {other:?}"),
    }
    assert_eq!(counters.started.load(Ordering::SeqCst), 1);
    assert_eq!(counters.finished.load(Ordering::SeqCst), 1);
}

/// Чужой ключ: модуль рвёт соединение без ответа, `kernel()` называет причину.
#[tokio::test]
async fn a_wrong_key_is_refused_and_reported() {
    for via in BOTH {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ServiceConfig {
            kernel_wait: HumanDuration(Duration::from_millis(300)),
            ..config(&dir, via)
        };
        let (listening, address) = listen(Module::new(), &cfg).await;

        let wrong = Psk::derive("another key of the service, 32+ chars");
        let res = transport::dial(&address, &wrong, &Config::default()).await;
        assert!(
            matches!(&res, Err(TransportError::Noise(m)) if m.contains("wrong key")),
            "{via:?}: {:?}",
            res.err()
        );

        match listening.kernel().await {
            Err(ConnectError::Transport(message)) => {
                assert!(message.contains("did not connect"), "{message}");
                assert!(message.contains("wrong key"), "{message}");
            }
            other => panic!("{via:?}: expected a timeout, got {other:?}"),
        }
        assert!(listening.module().is_none());
    }
}

#[tokio::test]
async fn kernel_wait_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = ServiceConfig {
        kernel_wait: HumanDuration(Duration::from_millis(100)),
        ..config(&dir, Via::Uds)
    };
    let (listening, _) = listen(Module::new(), &cfg).await;
    let res = tokio::time::timeout(Duration::from_secs(5), listening.kernel())
        .await
        .expect("kernel() gives up by itself");
    match res {
        Err(ConnectError::Transport(message)) => {
            assert!(message.contains("did not connect within"), "{message}");
        }
        other => panic!("expected a timeout, got {other:?}"),
    }
}

/// Поток молчащих сокетов не запирает ядро и не держит дескрипторы до
/// `handshake_timeout`: сверх лимита на сеть новичок вытесняет старейший.
#[tokio::test]
async fn a_flood_of_silent_sockets_does_not_lock_the_kernel_out() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = ServiceConfig {
        transport: Config {
            handshake_timeout: HumanDuration(Duration::from_secs(60)),
            ..Config::default()
        },
        ..config(&dir, Via::Tcp)
    };
    let (listening, address) = listen(Module::new(), &cfg).await;
    let ServiceAddress::Tcp { host, port } = &address else {
        panic!("tcp expected: {address}");
    };
    let mut flood = Vec::new();
    for _ in 0..3 * MAX_PENDING_PER_NET {
        let silent = tokio::net::TcpStream::connect((host.as_str(), *port))
            .await
            .unwrap();
        flood.push(silent);
    }

    let _session = dial(&address, fake::welcome("s1", &[]), Surfaces::default()).await;
    listening.kernel().await.expect("the kernel got through");

    let mut buf = [0; 1];
    let read = tokio::time::timeout(Duration::from_secs(5), flood[0].read(&mut buf))
        .await
        .expect("the oldest silent socket is evicted");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
}

/// Сокет, молчащий вместо Noise, закрывается по `handshake_timeout` и не
/// мешает ядру подключиться тем временем.
#[tokio::test]
async fn a_silent_socket_neither_blocks_the_kernel_nor_lingers() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = ServiceConfig {
        transport: Config {
            handshake_timeout: HumanDuration(Duration::from_millis(200)),
            ..Config::default()
        },
        ..config(&dir, Via::Tcp)
    };
    let (listening, address) = listen(Module::new(), &cfg).await;
    let ServiceAddress::Tcp { host, port } = &address else {
        panic!("tcp expected: {address}");
    };
    let mut silent = tokio::net::TcpStream::connect((host.as_str(), *port))
        .await
        .unwrap();

    let _session = dial(&address, fake::welcome("s1", &[]), Surfaces::default()).await;
    listening.kernel().await.expect("the kernel got through");

    let mut buf = [0; 1];
    let read = tokio::time::timeout(Duration::from_secs(5), silent.read(&mut buf))
        .await
        .expect("the module drops the silent socket");
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
}

/// `shutdown`: соединение закрыто `unavailable`, файл сокета убран, новый
/// дозвон не проходит, `Kernel` отвечает сразу.
#[tokio::test]
async fn shutdown_stops_listening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("module.sock");
    let (listening, address) = listen(Module::new(), &config(&dir, Via::Uds)).await;
    let session = dial(&address, fake::welcome("s1", &[]), surface_x("X")).await;
    let kernel = listening.kernel().await.unwrap();

    listening.run(async {}).await.unwrap();
    let reason = tokio::time::timeout(Duration::from_secs(5), session.peer.closed())
        .await
        .expect("shutdown closes the connection");
    assert_eq!(reason.code, ErrorCode::Unavailable);
    assert!(!path.exists(), "the socket file is removed");
    assert!(
        transport::dial(&address, &Psk::derive(KEY), &Config::default())
            .await
            .is_err()
    );

    let after = tokio::time::timeout(Duration::from_secs(1), get(kernel))
        .await
        .expect("nothing to wait for after shutdown");
    assert!(
        matches!(after, Err(Generic::Aux(Aux::Unavailable(_)))),
        "{after:?}"
    );
}

/// Последний клон `Listening` брошен без `run`: приём останавливается.
#[tokio::test]
async fn dropping_the_listener_stops_accepting() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("module.sock");
    let (listening, _) = listen(Module::new(), &config(&dir, Via::Uds)).await;
    drop(listening);
    tokio::time::timeout(Duration::from_secs(5), async {
        while path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the socket file is removed");
}
