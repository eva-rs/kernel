//! Форма секции `modules` (`06 §11`): дефолты и слушатели по ключу адреса.

use std::time::Duration;

use serde_json::json;

use eva::bytesize::ByteSize;
use eva_rpc::config::{Config, Listen, ServiceListen, StreamBuffer};

#[test]
fn defaults() {
    let cfg: Config = serde_json::from_value(json!({})).unwrap();
    assert_eq!(cfg, Config::default());
    assert_eq!(*cfg.handshake_timeout, Duration::from_secs(10));
    assert_eq!(*cfg.ping_interval, Duration::from_secs(15));
    assert_eq!(*cfg.ping_timeout, Duration::from_secs(45));
    assert_eq!(*cfg.request_timeout, Duration::from_secs(60));
    assert_eq!(*cfg.stream_idle_timeout, Duration::from_secs(120));
    assert_eq!(cfg.stream_buffer(), 64);
    assert_eq!(cfg.max_frame, ByteSize::mib(16));
    assert_eq!(cfg.max_hello, ByteSize::mib(4));
    assert_eq!(cfg.blob_chunk, ByteSize::mib(1));
    assert_eq!(*cfg.idempotency_window, Duration::from_secs(600));

    let cfg: Config = serde_json::from_value(json!({
        "ping_interval": "5s",
        "max_frame": "8 MiB",
        "stream_buffer": 16,
    }))
    .unwrap();
    assert_eq!(*cfg.ping_interval, Duration::from_secs(5));
    assert_eq!(cfg.max_frame, ByteSize::mib(8));
    assert_eq!(cfg.stream_buffer, StreamBuffer::POS16);

    // `hello` не бывает больше кадра.
    let cfg = Config {
        max_frame: ByteSize::mib(2),
        ..Config::default()
    };
    assert_eq!(cfg.max_hello(), 2 << 20);
    assert!(serde_json::from_value::<Config>(json!({"stream_buffer": 0})).is_err());
}

#[test]
fn listen_by_address_key() {
    let listen: Vec<Listen> = serde_json::from_value(json!([
        {"uds": "/run/eva-kernel/modules.sock"},
        {"ws": "127.0.0.1:9000"},
        {"wss": "0.0.0.0:9443", "tls": {"cert": "/etc/eva/cert.pem", "key": "/etc/eva/key.pem"}},
    ]))
    .unwrap();

    let [Listen::Uds(uds), Listen::Ws(ws), Listen::Wss(wss)] = listen.as_slice() else {
        panic!("wrong variants: {listen:?}");
    };
    assert_eq!(uds.path.to_str(), Some("/run/eva-kernel/modules.sock"));
    assert_eq!(ws.addr.port(), 9000);
    assert_eq!(wss.tls.key.to_str(), Some("/etc/eva/key.pem"));

    assert!(serde_json::from_value::<Listen>(json!({"tcp": "127.0.0.1:1"})).is_err());
}

/// Сервис слушает uds или tcp; ws/wss — только у клиентского слушателя ядра.
#[test]
fn service_listen_by_address_key() {
    let listen: Vec<ServiceListen> = serde_json::from_value(json!([
        {"uds": "/run/eva-bridge/module.sock"},
        {"tcp": "0.0.0.0:7000"},
    ]))
    .unwrap();
    let [ServiceListen::Uds(uds), ServiceListen::Tcp(tcp)] = listen.as_slice() else {
        panic!("wrong variants: {listen:?}");
    };
    assert_eq!(uds.path.to_str(), Some("/run/eva-bridge/module.sock"));
    assert_eq!(tcp.addr.port(), 7000);

    // Слушатель привязывается к IP, а не к имени хоста.
    for other in [
        json!({"ws": "127.0.0.1:1"}),
        json!({"tcp": "host"}),
        json!({"tcp": "localhost:7000"}),
    ] {
        assert!(
            serde_json::from_value::<ServiceListen>(other.clone()).is_err(),
            "{other}"
        );
    }
}

/// Ответ и элемент потока с данными в `max_payload` укладываются в `max_frame`.
#[test]
fn max_payload_leaves_room_for_the_envelope() {
    use eva_protocol::{
        types::Bytes,
        wire::{
            Frame,
            frame::{Response, StreamItem},
        },
    };
    use eva_rpc::codec::encode_frame;

    let cfg = Config {
        max_frame: ByteSize::kib(64),
        ..Config::default()
    };
    let data = Bytes(vec![0; usize::try_from(cfg.max_payload()).unwrap()]);
    let payload = eva_rpc::codec::encode(&data).unwrap();
    for frame in [
        Frame::Response(Response::ok(u64::MAX, payload.clone())),
        Frame::StreamItem(StreamItem {
            id: u64::MAX,
            seq: u64::MAX,
            item: payload,
        }),
    ] {
        let len = encode_frame(&frame).unwrap().len();
        assert!(len <= cfg.max_frame(), "{len} > {}", cfg.max_frame());
    }
}
