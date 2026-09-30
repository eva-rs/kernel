//! Дозвон сервисов реестра (`06 §2.1`): `modules.add` звонит сразу, по uds
//! и tcp; `modules.remove` закрывает сессию, гасит петлю и не отдаёт сессию
//! попытке в полёте; паузы петли.

mod common;

use std::time::Duration;

use common::{Harness, bind_keyed, dialed};
use eva_core::{CallStep as _, service::modules::Modules as _};
use eva_kernel::subsystems::modules::dial::Backoff;
use eva_protocol::{
    requests::modules::{add, list, remove},
    types::{
        ModuleId,
        module::{ModuleKind, ServiceAddress, ServiceKey, State},
    },
    wire::{Capabilities, ErrorCode},
};
use eva_rpc::{
    ConnectError, NoHandler, RpcClient, ServiceListener,
    config::{FileMode, ServiceListen, TcpListen, UdsListen},
};

const KEY: &str = "bridge-key-0123456789abcdef0123456789";
const WAIT: Duration = Duration::from_secs(5);

fn uds(h: &Harness, name: &str) -> ServiceListen {
    ServiceListen::Uds(UdsListen {
        path: common::service_socket(h.dir.path(), name),
        mode: FileMode::default(),
        group: None,
    })
}

async fn add(h: &Harness, module: &str, address: ServiceAddress, key: Option<&str>) -> add::Ok {
    h.client_session()
        .await
        .modules()
        .add()
        .call(add::Args {
            module: ModuleId::from(module),
            address,
            key: key.map(ServiceKey::from),
        })
        .await
        .expect("modules.add")
}

async fn remove(h: &Harness, module: &str) {
    h.client_session()
        .await
        .modules()
        .remove()
        .call(remove::Args {
            module: ModuleId::from(module),
        })
        .await
        .expect("modules.remove");
}

async fn state(h: &Harness, module: &str) -> Option<State> {
    h.client_session()
        .await
        .modules()
        .list()
        .call(list::Args {})
        .await
        .expect("modules.list")
        .modules
        .into_iter()
        .find(|m| m.name.as_str() == module)
        .map(|m| m.state)
}

/// Ядро больше не звонит на `listener`.
async fn silent(listener: &ServiceListener) {
    let redial = tokio::time::timeout(Duration::from_secs(2), listener.accept()).await;
    assert!(redial.is_err(), "the kernel dials a removed module");
}

async fn add_dials_at_once_and_remove_closes(listen: ServiceListen, h: &Harness) {
    let listener = bind_keyed(&listen, KEY).await;
    let address = listener.local_addr().expect("listener address");

    // Раньше минимальной паузы петли: звонок от самого `add`, не от повтора.
    let started = tokio::time::Instant::now();
    let add::Ok { key } = add(h, "bridge", address, Some(KEY)).await;
    assert_eq!(key, None);
    let (bridge, welcome) = dialed(&listener, Capabilities::default(), NoHandler).await;
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "dialed {:?} after add",
        started.elapsed()
    );
    assert_eq!(
        (welcome.module.as_str(), welcome.kind),
        ("bridge", ModuleKind::Service)
    );
    assert_eq!(state(h, "bridge").await, Some(State::Connected));

    remove(h, "bridge").await;
    let reason = tokio::time::timeout(WAIT, bridge.closed())
        .await
        .expect("the session closes");
    assert_eq!(
        (reason.code, reason.message.as_str()),
        (ErrorCode::Unauthorized, "module removed")
    );
    silent(&listener).await;
    assert_eq!(state(h, "bridge").await, Some(State::Offline));
}

#[tokio::test]
async fn a_service_added_over_uds_is_dialed_and_removed() {
    let h = common::start().await;
    let (_listening, _url) = h.listen().await;
    add_dials_at_once_and_remove_closes(uds(&h, "bridge"), &h).await;
}

#[tokio::test]
async fn a_service_added_over_tcp_is_dialed_and_removed() {
    let h = common::start().await;
    let (_listening, _url) = h.listen().await;
    let listen = ServiceListen::Tcp(TcpListen {
        addr: "127.0.0.1:0".parse().expect("loopback"),
    });
    add_dials_at_once_and_remove_closes(listen, &h).await;
}

/// Ключ, сгенерированный ядром, — тот, с которым оно звонит.
#[tokio::test]
async fn a_generated_key_is_the_one_the_kernel_dials_with() {
    let h = common::start().await;
    let (_listening, _url) = h.listen().await;
    let path = common::service_socket(h.dir.path(), "bridge");

    let add::Ok { key } = add(&h, "bridge", ServiceAddress::Uds(path), None).await;
    let key = key.expect("generated");
    let listener = bind_keyed(&uds(&h, "bridge"), key.expose()).await;
    let (_bridge, welcome) = dialed(&listener, Capabilities::default(), NoHandler).await;
    assert_eq!(welcome.module.as_str(), "bridge");
}

/// `modules.remove` посреди попытки: сессии нет, повторов нет (`06 §2.1`).
#[tokio::test]
async fn a_dial_in_flight_gets_no_session_after_remove() {
    let h = common::start().await;
    let (_listening, _url) = h.listen().await;
    let listener = bind_keyed(&uds(&h, "bridge"), KEY).await;
    let address = listener.local_addr().expect("listener address");
    add(&h, "bridge", address, Some(KEY)).await;

    let incoming = tokio::time::timeout(WAIT, listener.accept())
        .await
        .expect("the kernel dials")
        .expect("accept");
    remove(&h, "bridge").await;

    let conn = incoming.secure().await.expect("noise");
    let res = RpcClient::answer(
        conn,
        common::dialed_hello(Capabilities::default()),
        eva_rpc::ClientOptions::default(),
        NoHandler,
    )
    .await;
    let Err(ConnectError::Rejected(error)) = res else {
        panic!("expected a refused hello, got {res:?}");
    };
    assert_eq!(error.code, ErrorCode::Unauthorized, "{error:?}");
    assert_eq!(state(&h, "bridge").await, None, "no session, no row");
    silent(&listener).await;
}

/// Обрыв сессии — offline и повтор по паузе: ядро перезванивает само.
#[tokio::test]
async fn the_kernel_redials_after_a_lost_session() {
    let h = common::start().await;
    let (_listening, _url) = h.listen().await;
    let listener = bind_keyed(&uds(&h, "bridge"), KEY).await;
    let address = listener.local_addr().expect("listener address");
    add(&h, "bridge", address, Some(KEY)).await;

    let (first, welcome) = dialed(&listener, Capabilities::default(), NoHandler).await;
    drop(first);
    let (_second, again) = dialed(&listener, Capabilities::default(), NoHandler).await;
    assert_eq!(again.module, welcome.module);
    assert_ne!(again.session_id, welcome.session_id);
    assert_eq!(state(&h, "bridge").await, Some(State::Connected));
}

#[test]
fn backoff_doubles_to_a_minute_with_jitter_and_resets() {
    let mut backoff = Backoff::default();
    let mut ceiling = Duration::from_secs(1);
    for _ in 0..10 {
        let pause = backoff.pause();
        assert!(
            pause >= ceiling / 2 && pause <= ceiling,
            "{pause:?} outside [{:?}, {ceiling:?}]",
            ceiling / 2
        );
        ceiling = (ceiling * 2).min(Duration::from_secs(60));
    }
    assert_eq!(ceiling, Duration::from_secs(60));

    backoff.reset();
    assert!(backoff.pause() <= Duration::from_secs(1));
}
