//! Socket activation: слушатель берёт сокет менеджера служб с тем же адресом
//! вместо bind; права его файла задаёт менеджер, а не процесс.

use std::{
    os::{fd::OwnedFd, unix::net::UnixListener},
    path::Path,
};

use eva_rpc::{
    Inherited, Listen, Listener, NoHandler, Psk, RpcClient, ServiceListen, ServiceListener,
    TransportError,
    config::{UdsListen, WsListen},
};

use crate::{
    harness::{Harness, KEY, Via, config, service_hello},
    kernel::SERVICE,
};

fn uds(path: &Path) -> ServiceListen {
    ServiceListen::Uds(UdsListen {
        path: path.to_path_buf(),
    })
}

fn inherited(listener: UnixListener) -> Inherited {
    Inherited::from_fds([OwnedFd::from(listener)])
}

/// Унаследованный сокет обслуживается как свой, но файл после закрытия
/// остаётся менеджеру.
#[tokio::test(flavor = "multi_thread")]
async fn an_inherited_service_socket_is_served_and_kept() {
    let h = Harness::start(Via::Uds).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("activated.sock");
    let mut sockets = inherited(UnixListener::bind(&path).unwrap());
    let listener = ServiceListener::open(&uds(&path), &mut sockets, Psk::derive(KEY), &h.config)
        .await
        .expect("the inherited socket is taken, not re-bound");
    sockets.finish().expect("every inherited socket is claimed");
    let address = listener.local_addr().unwrap();
    assert_eq!(address.path(), Some(path.as_path()));

    let options = h.options();
    let module = tokio::spawn(async move {
        let conn = listener.accept().await.unwrap().secure().await.unwrap();
        let answered = RpcClient::answer(conn, service_hello(), options, NoHandler).await;
        drop(listener);
        answered
    });
    let accepted = h
        .server
        .dial(&address, &Psk::derive(KEY), &SERVICE.into())
        .await
        .unwrap();
    let (_module, welcome) = module.await.unwrap().unwrap();
    assert_eq!(welcome.session_id, accepted.welcome.session_id);
    assert!(
        path.exists(),
        "an inherited socket belongs to the service manager"
    );
}

/// `Accept=yes` отдаёт принятое соединение с адресом слушателя: это ошибка
/// старта, а не слушатель, на котором `accept` крутился бы вхолостую.
#[tokio::test]
async fn an_inherited_connection_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("activated.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let _client = std::os::unix::net::UnixStream::connect(&path).unwrap();
    let (accepted, _) = listener.accept().unwrap();
    let mut sockets = Inherited::from_fds([OwnedFd::from(accepted)]);

    let error = ServiceListener::open(&uds(&path), &mut sockets, Psk::derive(KEY), &config())
        .await
        .err()
        .expect("a connected socket is not a listener");
    assert!(
        matches!(&error, TransportError::Activation(m) if m.contains("Accept=no")),
        "{error:?}"
    );
}

/// Сокет, которого не называет конфиг, некому обслужить — ошибка старта.
#[tokio::test]
async fn an_unnamed_inherited_socket_fails_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let (named, stray) = (dir.path().join("named.sock"), dir.path().join("stray.sock"));
    let mut sockets = Inherited::from_fds([
        OwnedFd::from(UnixListener::bind(&named).unwrap()),
        OwnedFd::from(UnixListener::bind(&stray).unwrap()),
    ]);

    let _listener = ServiceListener::open(&uds(&named), &mut sockets, Psk::derive(KEY), &config())
        .await
        .unwrap();
    let error = sockets.finish().unwrap_err();
    let TransportError::Activation(message) = &error else {
        panic!("expected an activation error, got {error:?}");
    };
    assert!(message.contains("stray.sock"), "{message}");
    assert!(!message.contains("named.sock"), "{message}");
}

/// Слушатели клиентов ядра узнают свои сокеты по адресу: uds по пути, ws по
/// `ip:port`; свой bind на drop убирает файл, унаследованный — нет.
#[tokio::test(flavor = "multi_thread")]
async fn kernel_listeners_take_inherited_sockets_by_address() {
    let cfg = config();
    let dir = tempfile::tempdir().unwrap();
    let inherited_path = dir.path().join("inherited.sock");
    let bound_path = dir.path().join("bound.sock");
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = tcp.local_addr().unwrap();
    let mut sockets = Inherited::from_fds([
        OwnedFd::from(tcp),
        OwnedFd::from(UnixListener::bind(&inherited_path).unwrap()),
    ]);

    let uds = |path: &Path| {
        Listen::Uds(UdsListen {
            path: path.to_path_buf(),
        })
    };
    let from_manager = Listener::open(&uds(&inherited_path), &mut sockets, &cfg)
        .await
        .unwrap();
    let own = Listener::open(&uds(&bound_path), &mut sockets, &cfg)
        .await
        .unwrap();
    let ws = Listener::open(&Listen::Ws(WsListen { addr }), &mut sockets, &cfg)
        .await
        .unwrap();
    sockets.finish().expect("every inherited socket is claimed");

    assert!(from_manager.inherited());
    assert!(!own.inherited());
    assert!(ws.inherited());
    assert_eq!(ws.local_addr(), Some(addr));

    drop((from_manager, own, ws));
    assert!(inherited_path.exists(), "the manager's socket file stays");
    assert!(!bound_path.exists(), "an own socket file is removed");
}
