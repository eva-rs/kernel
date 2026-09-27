//! Протокол руками: handshake, ping, окно идемпотентности — сырыми кадрами по uds.

use std::time::Duration;

use eva_protocol::{
    requests::{surfaces::create, turns::attach},
    wire::{
        ErrorCode, Frame, RpcError,
        frame::{Notify, Response},
        handshake::Ping,
        method,
    },
};
use eva_rpc::{
    ClientOptions, Conn, IdSeq, NoHandler, RpcClient, codec, config::Config, transport::Outbound,
};

use crate::{
    harness::{Harness, Via, config, hello, ms, resume},
    kernel::{CLIENT_TOKEN, OTHER_TOKEN, chat, chat_id},
};

fn ping() -> Frame {
    Frame::Notify(Notify {
        method: method::PING.to_owned(),
        params: codec::encode(&Ping {}).unwrap(),
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn frames_before_welcome_close() {
    let h = Harness::start(Via::Uds).await;

    let mut raw = h.raw().await;
    raw.send(&ping()).await;
    assert!(raw.closed().await);

    let mut raw = h.raw().await;
    raw.request(3, "surfaces.get", codec::encode(&()).unwrap())
        .await;
    match raw.recv().await {
        Some(Frame::Response(response)) => {
            assert_eq!(response.id, 3);
            assert_eq!(
                response.into_result().unwrap_err().code,
                ErrorCode::InvalidParams
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(raw.closed().await);

    // Со стороны клиента: кадр до `welcome` — отказ handshake.
    let cfg = config();
    let (client_end, mut kernel_end) = Conn::pair(&cfg);
    let fake = tokio::spawn(async move {
        let _hello = kernel_end.recv().await;
        kernel_end
            .send(Outbound::Frame(codec::encode_frame(&ping()).unwrap()))
            .await
            .unwrap();
        kernel_end.recv().await
    });
    let result = RpcClient::handshake(
        client_end,
        hello(CLIENT_TOKEN),
        Default::default(),
        NoHandler,
    )
    .await;
    assert!(
        matches!(result, Err(eva_rpc::ConnectError::Handshake(_))),
        "{result:?}"
    );
    // Клиент закрыл соединение.
    assert!(matches!(fake.await.unwrap(), None | Some(Err(_))));
}

#[tokio::test(flavor = "multi_thread")]
async fn handshake_timeout_closes() {
    let cfg = Config {
        handshake_timeout: ms(100),
        ..config()
    };
    let h = Harness::with(Via::Uds, cfg).await;
    let mut raw = h.raw().await;
    assert!(raw.closed().await);
}

#[tokio::test(flavor = "multi_thread")]
async fn ping_timeout_closes() {
    let cfg = Config {
        ping_interval: ms(50),
        ping_timeout: ms(200),
        ..config()
    };
    let mut h = Harness::with(Via::Uds, cfg).await;

    let mut raw = h.raw().await;
    raw.hello(&hello(CLIENT_TOKEN)).await.unwrap();
    let accepted = h.accepted().await;

    // Молчащий пир: ядро шлёт ping, но ответа нет.
    let reason = tokio::time::timeout(Duration::from_secs(5), accepted.peer.closed())
        .await
        .unwrap();
    assert_eq!(reason.code, ErrorCode::Timeout);
    assert!(raw.closed().await);
}

#[tokio::test(flavor = "multi_thread")]
async fn pinging_peer_stays() {
    let cfg = Config {
        ping_interval: ms(50),
        ping_timeout: ms(200),
        ..config()
    };
    let mut h = Harness::with(Via::Uds, cfg).await;
    let (client, accepted) = h.connect(CLIENT_TOKEN).await;

    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!client.peer().is_closed());
    assert!(!accepted.peer.is_closed());
}

/// `close` приходит раньше конца отдаваемого потока: иначе пир увидел бы
/// конец потока при ещё открытом соединении и принял обрыв за ошибку потока.
#[tokio::test(flavor = "multi_thread")]
async fn close_precedes_the_ends_of_served_streams() {
    let mut h = Harness::start(Via::Uds).await;
    let mut raw = h.raw().await;
    raw.hello(&hello(CLIENT_TOKEN)).await.unwrap();
    let accepted = h.accepted().await;

    let args = attach::Args {
        chat: chat_id(chat::ENDLESS),
        since_seq: None,
    };
    raw.request(5, "turns.attach", codec::encode(&args).unwrap())
        .await;
    loop {
        match raw.recv().await {
            Some(Frame::StreamHeartbeat(_)) => {}
            Some(Frame::StreamItem(item)) if item.id == 5 => break,
            other => panic!("expected the first item, got {other:?}"),
        }
    }
    accepted
        .peer
        .close(RpcError::new(ErrorCode::Unavailable, "kernel restarts"));

    let mut frames = Vec::new();
    while let Some(frame) = raw.recv().await {
        frames.push(frame);
    }
    let close = frames
        .iter()
        .position(|f| matches!(f, Frame::Notify(n) if n.method == method::CLOSE));
    let end = frames
        .iter()
        .position(|f| matches!(f, Frame::StreamEnd(end) if end.id == 5));
    assert!(
        matches!((close, end), (Some(close), Some(end)) if close < end),
        "{frames:?}"
    );
}

fn create(id: &str) -> create::Args {
    create::Args {
        id: id.into(),
        name: id.to_owned(),
        rules: None,
    }
}

fn decode_created(
    result: Result<eva_protocol::wire::frame::Payload, RpcError>,
) -> Result<create::Ok, RpcError> {
    result.map(|ok| codec::decode(&ok).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn idempotent_retry() {
    let h = Harness::start(Via::Uds).await;
    let creates = || h_creates(&h);

    let mut raw = h.raw().await;
    let welcome = raw.hello(&hello(CLIENT_TOKEN)).await.unwrap();
    assert!(!welcome.resumed);
    let params = codec::encode(&create("idem")).unwrap();

    raw.request(7, "surfaces.create", params.clone()).await;
    let first = decode_created(raw.response(7).await).unwrap();
    raw.request(7, "surfaces.create", params.clone()).await;
    let again = decode_created(raw.response(7).await).unwrap();
    assert_eq!(first, again);
    assert_eq!(creates(), 1);

    // Тот же id другим методом — не повтор.
    raw.request(7, "surfaces.update", params.clone()).await;
    assert_eq!(
        raw.response(7).await.unwrap_err().code,
        ErrorCode::InvalidParams
    );

    // `hello.resume` переносит окно в новую сессию.
    let mut resumed = h.raw().await;
    let continued = resumed
        .hello(&resume(hello(CLIENT_TOKEN), &welcome.session_id))
        .await
        .unwrap();
    assert!(continued.resumed);
    resumed.request(7, "surfaces.create", params.clone()).await;
    assert_eq!(decode_created(resumed.response(7).await).unwrap(), first);
    assert_eq!(creates(), 1);

    // Без `resume` тот же id — новый запрос.
    let mut fresh = h.raw().await;
    assert!(!fresh.hello(&hello(CLIENT_TOKEN)).await.unwrap().resumed);
    fresh.request(7, "surfaces.create", params).await;
    assert_eq!(
        fresh.response(7).await.unwrap_err().code,
        ErrorCode::Conflict
    );
    assert_eq!(creates(), 2);

    // Чужой модуль не подхватит окно по `resume`.
    let mut thief = h.raw().await;
    let stolen = thief
        .hello(&resume(hello(OTHER_TOKEN), &welcome.session_id))
        .await
        .unwrap();
    assert!(!stolen.resumed);
    thief
        .request(
            7,
            "surfaces.create",
            codec::encode(&create("other")).unwrap(),
        )
        .await;
    assert!(thief.response(7).await.is_ok());
    assert_eq!(creates(), 3);
}

/// `Peer::call_with_id` на сессии `hello.resume`: мутация тем же id не
/// исполняется снова, ответ — из окна.
#[tokio::test(flavor = "multi_thread")]
async fn call_with_id_resends_into_the_window() {
    let mut h = Harness::start(Via::Uds).await;
    let (client, _) = h.connect(CLIENT_TOKEN).await;
    let params = codec::encode(&create("pinned")).unwrap();
    let id = client.peer().ids().next();
    let first = client
        .peer()
        .call_with_id(id, "surfaces.create", params.clone())
        .await;
    let first = decode_created(first).unwrap();

    let options = ClientOptions {
        ids: client.peer().ids().clone(),
        ..h.options()
    };
    let hello = resume(hello(CLIENT_TOKEN), &client.welcome().session_id);
    let (resumed, _) = RpcClient::connect_with(&h.target, hello, options, NoHandler)
        .await
        .unwrap();
    let again = resumed
        .peer()
        .call_with_id(id, "surfaces.create", params)
        .await;
    assert_eq!(decode_created(again).unwrap(), first);
    assert_eq!(h_creates(&h), 1);
}

fn h_creates(h: &Harness) -> usize {
    h.kernel
        .state
        .creates
        .load(std::sync::atomic::Ordering::SeqCst)
}

#[tokio::test(flavor = "multi_thread")]
async fn retry_waits_for_the_running_original() {
    let h = Harness::start(Via::Uds).await;
    *h.kernel.state.create_delay.lock().unwrap() = Duration::from_millis(300);

    let mut first = h.raw().await;
    let welcome = first.hello(&hello(CLIENT_TOKEN)).await.unwrap();
    let params = codec::encode(&create("slow")).unwrap();
    first.request(11, "surfaces.create", params.clone()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Обрыв посреди мутации: исполнение доживает, итог ложится в окно.
    drop(first);

    let mut again = h.raw().await;
    again
        .hello(&resume(hello(CLIENT_TOKEN), &welcome.session_id))
        .await
        .unwrap();
    again.request(11, "surfaces.create", params).await;
    let ok = decode_created(again.response(11).await).unwrap();
    assert_eq!(ok.surface.id.as_str(), "slow");
    assert_eq!(h_creates(&h), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_are_not_cached() {
    let h = Harness::start(Via::Uds).await;
    let mut raw = h.raw().await;
    raw.hello(&hello(CLIENT_TOKEN)).await.unwrap();

    let get = codec::encode(&eva_protocol::requests::surfaces::get::Args {
        surface: "late".into(),
    })
    .unwrap();
    raw.request(5, "surfaces.get", get.clone()).await;
    assert_eq!(raw.response(5).await.unwrap_err().code, ErrorCode::NotFound);

    raw.request(
        6,
        "surfaces.create",
        codec::encode(&create("late")).unwrap(),
    )
    .await;
    raw.response(6).await.unwrap();

    raw.request(5, "surfaces.get", get).await;
    assert!(raw.response(5).await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_frame_kind_is_skipped() {
    let mut h = Harness::start(Via::Uds).await;
    let mut raw = h.raw().await;
    raw.hello(&hello(CLIENT_TOKEN)).await.unwrap();
    let accepted = h.accepted().await;

    // Вид кадра новее этого пира: `{"telepathy": {...}}`.
    let future = rmp_serde::to_vec_named(&serde_json::json!({"telepathy": {"id": 1}})).unwrap();
    raw.send_bytes(&(future.len() as u32).to_be_bytes()).await;
    raw.send_bytes(&future).await;

    raw.request(
        9,
        "surfaces.get",
        codec::encode(&serde_json::json!({"surface": "x"})).unwrap(),
    )
    .await;
    assert_eq!(raw.response(9).await.unwrap_err().code, ErrorCode::NotFound);
    assert!(!accepted.peer.is_closed());
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_frame_closes_uds() {
    let cfg = Config {
        max_frame: eva::bytesize::ByteSize::kib(64),
        ..config()
    };
    let mut h = Harness::with(Via::Uds, cfg).await;
    let mut raw = h.raw().await;
    raw.hello(&hello(CLIENT_TOKEN)).await.unwrap();
    let accepted = h.accepted().await;

    // Заголовок кадра в мегабайт при лимите 64 KiB.
    raw.send_bytes(&(1_u32 << 20).to_be_bytes()).await;
    let reason = tokio::time::timeout(Duration::from_secs(5), accepted.peer.closed())
        .await
        .unwrap();
    assert_eq!(reason.code, ErrorCode::InvalidParams);
    assert!(raw.closed().await);
}

#[test]
fn server_ids_do_not_meet_client_ids() {
    let server = IdSeq::server();
    let client = IdSeq::new();
    assert!(server.next() >= 1 << 63);
    assert_eq!(client.next(), 0);
}

#[test]
fn response_carries_exactly_one() {
    let both = Response {
        id: 1,
        result: Some(Vec::new().into()),
        error: Some(RpcError::cancelled()),
    };
    assert_eq!(
        both.into_result().unwrap_err().code,
        ErrorCode::InvalidParams
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn live_socket_is_not_stolen() {
    let h = Harness::start(Via::Uds).await;
    let listen = eva_rpc::Listen::Uds(eva_rpc::config::UdsListen {
        path: h.socket.clone().unwrap(),
        mode: Default::default(),
        group: None,
    });
    let second = eva_rpc::Listener::bind(&listen, &h.config).await;
    assert!(second.is_err());

    // Первый слушатель жив.
    let mut raw = h.raw().await;
    raw.hello(&hello(CLIENT_TOKEN)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn ws_serves_only_the_modules_path() {
    let h = Harness::start(Via::Ws).await;
    let mut wrong = h.target.clone();
    wrong.set_path("/elsewhere");
    let result = RpcClient::connect_with(&wrong, hello(CLIENT_TOKEN), h.options(), NoHandler).await;
    assert!(
        matches!(result, Err(eva_rpc::ConnectError::Transport(_))),
        "{result:?}"
    );

    // Без пути клиент идёт на `/v1/modules` сам.
    let mut bare = h.target.clone();
    bare.set_path("");
    let result = RpcClient::connect_with(&bare, hello(CLIENT_TOKEN), h.options(), NoHandler).await;
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_capability_keys_are_warned() {
    let h = Harness::start(Via::Uds).await;
    let mut raw = h.raw().await;

    // Capability новее ядра: `hello` проходит, ключ возвращается в `warnings`.
    let mut params = serde_json::to_value(hello(CLIENT_TOKEN)).unwrap();
    params["capabilities"]["telepathy"] = serde_json::json!({"range": 3});
    params["capabilities"]["events"] = serde_json::json!({"subscriptions": []});
    raw.request(0, method::HELLO, codec::encode(&params).unwrap())
        .await;
    let welcome: eva_protocol::wire::Welcome =
        codec::decode(&raw.response(0).await.expect("welcome")).unwrap();

    let warned: Vec<_> = welcome
        .warnings
        .iter()
        .map(|w| (w.code, w.subject.as_str()))
        .collect();
    assert_eq!(
        warned,
        [(ErrorCode::InvalidParams, "capabilities.telepathy")]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn large_response_is_not_retained() {
    let h = Harness::start(Via::Uds).await;
    let mut raw = h.raw().await;
    raw.hello(&hello(CLIENT_TOKEN)).await.unwrap();

    let params = codec::encode(&create(&"b".repeat(100 * 1024))).unwrap();
    raw.request(3, "surfaces.create", params.clone()).await;
    decode_created(raw.response(3).await).unwrap();

    // Повтор не исполняется второй раз, но крупный итог окно не хранит.
    raw.request(3, "surfaces.create", params).await;
    assert_eq!(raw.response(3).await.unwrap_err().code, ErrorCode::Conflict);
    assert_eq!(h_creates(&h), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_over_max_inflight_get_quota() {
    let cfg = Config {
        max_inflight: eva_rpc::config::SessionLimit::POS1,
        ..config()
    };
    let h = Harness::with(Via::Uds, cfg).await;
    *h.kernel.state.create_delay.lock().unwrap() = Duration::from_millis(300);
    let mut raw = h.raw().await;
    raw.hello(&hello(CLIENT_TOKEN)).await.unwrap();

    raw.request(1, "surfaces.create", codec::encode(&create("a")).unwrap())
        .await;
    raw.request(2, "surfaces.create", codec::encode(&create("b")).unwrap())
        .await;
    assert_eq!(raw.response(2).await.unwrap_err().code, ErrorCode::Quota);
    decode_created(raw.response(1).await).unwrap();

    // Место освободилось.
    raw.request(3, "surfaces.create", codec::encode(&create("c")).unwrap())
        .await;
    decode_created(raw.response(3).await).unwrap();
}
