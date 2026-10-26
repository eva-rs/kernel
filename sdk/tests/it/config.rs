//! Секция `module` сервиса и клиента, пауза реконнекта.

use std::time::Duration;

use serde_json::json;

use eva::{time::HumanDuration, url::Url};
use eva_rpc::{Config, Conn, config::UdsListen};
use eva_sdk::{
    Backoff, ClientConfig, ConnectError, Module, SecretString, ServiceConfig, ServiceListen,
};

use crate::fake::{self, Surfaces};

#[test]
fn client_section_takes_defaults() {
    let cfg: ClientConfig = serde_json::from_value(json!({
        "soul": "uds:///run/eva-soul/modules.sock",
        "token": {"plaintext": "s3cret"},
    }))
    .unwrap();
    assert_eq!(cfg.soul.scheme(), "uds");
    assert_eq!(cfg.token.read().unwrap(), "s3cret");
    assert_eq!(*cfg.connect_wait, Duration::from_secs(180));
    assert!(cfg.tls.extra_ca.is_none());
    assert_eq!(*cfg.transport.ping_interval, Duration::from_secs(15));

    let cfg: ClientConfig = serde_json::from_value(json!({
        "soul": "wss://eva.example:8443/v1/modules",
        "token": {"file": "/run/credentials/x.service/token"},
        "connect_wait": "10s",
        "ping_interval": "5s",
        "tls": {"extra_ca": "/etc/eva/ca.pem"},
    }))
    .unwrap();
    assert_eq!(
        cfg.token,
        SecretString::File("/run/credentials/x.service/token".into())
    );
    assert_eq!(*cfg.connect_wait, Duration::from_secs(10));
    assert_eq!(*cfg.transport.ping_interval, Duration::from_secs(5));
    assert!(cfg.tls.extra_ca.is_some());
}

#[test]
fn token_is_required_and_tagged() {
    let res = serde_json::from_value::<ClientConfig>(json!({"soul": "uds:///x"}));
    assert!(res.is_err());
    let bare = serde_json::from_value::<ClientConfig>(json!({
        "soul": "uds:///x",
        "token": "s3cret",
    }));
    assert!(bare.is_err(), "a secret names its form: plaintext or file");
}

#[test]
fn token_never_prints() {
    let cfg: ClientConfig = serde_json::from_value(json!({
        "soul": "uds:///x",
        "token": {"plaintext": "s3cret"},
    }))
    .unwrap();
    assert!(!format!("{cfg:?}").contains("s3cret"));
    assert!(!format!("{}", cfg.token).contains("s3cret"));
}

/// Файл секрета с переводом строки: токен в `hello` без него.
#[tokio::test]
async fn a_file_token_is_read_without_its_trailing_newline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, "s3cret\n").unwrap();

    let (module_end, soul_end) = Conn::pair(&Config::default());
    let soul = tokio::spawn(fake::accept(
        soul_end,
        fake::welcome("s1", &[]),
        Surfaces::default(),
    ));
    let _connected = Module::new()
        .handshake(module_end, &SecretString::File(path))
        .await
        .expect("handshake");
    let session = soul.await.unwrap();
    assert_eq!(
        session.hello.token.as_ref().map(|t| t.expose()),
        Some("s3cret")
    );
}

/// Нечитаемый секрет — ошибка старта с ключом конфига, до сети дело не доходит.
#[tokio::test]
async fn an_unreadable_token_names_its_key() {
    let dir = tempfile::tempdir().unwrap();
    let cfg: ClientConfig = serde_json::from_value(json!({
        "soul": format!("uds://{}", dir.path().join("none.sock").display()),
        "token": {"file": dir.path().join("missing")},
    }))
    .unwrap();
    let res = Module::new()
        .connect_wait(Duration::from_secs(30))
        .connect_config(&cfg)
        .await;
    match res {
        Err(ConnectError::Config(message)) => {
            assert!(message.starts_with("module.token:"), "{message}");
        }
        other => panic!("expected a config error, got {other:?}"),
    }

    let url = Url::parse("uds:///nowhere.sock").unwrap();
    let res = Module::new()
        .connect(&url, &SecretString::File(dir.path().join("missing")))
        .await;
    assert!(matches!(res, Err(ConnectError::Config(_))), "{res:?}");
}

#[test]
fn backoff_doubles_and_caps() {
    let mut backoff = Backoff::default();
    let delays: Vec<u64> = (0..9).map(|_| backoff.next_delay().as_secs()).collect();
    assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    backoff.reset();
    assert_eq!(backoff.next_delay(), Duration::from_secs(1));

    let mut tight = Backoff::new(Duration::from_millis(10), Duration::from_millis(25));
    let delays: Vec<u128> = (0..4).map(|_| tight.next_delay().as_millis()).collect();
    assert_eq!(delays, [10, 20, 25, 25]);
}

#[test]
fn service_section_takes_defaults() {
    let cfg: ServiceConfig = serde_json::from_value(json!({
        "listen": {"uds": "/run/eva-x/module.sock"},
        "key": {"file": "/run/credentials/x.service/key"},
    }))
    .unwrap();
    let ServiceListen::Uds(uds) = &cfg.listen else {
        panic!("uds expected: {:?}", cfg.listen);
    };
    assert_eq!(uds.path.to_str(), Some("/run/eva-x/module.sock"));
    assert_eq!(
        cfg.key,
        SecretString::File("/run/credentials/x.service/key".into())
    );
    assert_eq!(*cfg.soul_wait, Duration::from_secs(180));
    assert_eq!(*cfg.transport.handshake_timeout, Duration::from_secs(10));

    let cfg: ServiceConfig = serde_json::from_value(json!({
        "listen": {"tcp": "127.0.0.1:7001"},
        "key": {"plaintext": "k"},
        "soul_wait": "10s",
        "ping_interval": "5s",
    }))
    .unwrap();
    let ServiceListen::Tcp(tcp) = &cfg.listen else {
        panic!("tcp expected: {:?}", cfg.listen);
    };
    assert_eq!(tcp.addr.to_string(), "127.0.0.1:7001");
    assert_eq!(*cfg.soul_wait, Duration::from_secs(10));
    assert_eq!(*cfg.transport.ping_interval, Duration::from_secs(5));
}

#[test]
fn service_listen_and_key_are_required() {
    let key = json!({"plaintext": "k"});
    for section in [
        json!({"key": key}),
        json!({"listen": {"uds": "/x.sock"}}),
        json!({"listen": {"uds": "/x.sock"}, "key": "bare"}),
        json!({"listen": {"tcp": "127.0.0.1"}, "key": key}),
        json!({"listen": "uds:///x.sock", "key": key}),
    ] {
        let res = serde_json::from_value::<ServiceConfig>(section.clone());
        assert!(res.is_err(), "{section}");
    }
}

#[test]
fn service_key_never_prints() {
    let cfg: ServiceConfig = serde_json::from_value(json!({
        "listen": {"uds": "/x.sock"},
        "key": {"plaintext": "s3cret-key"},
    }))
    .unwrap();
    assert!(!format!("{cfg:?}").contains("s3cret-key"));
}

/// Негодный ключ — ошибка старта с ключом конфига; сокет не создаётся.
#[tokio::test]
async fn an_empty_or_unreadable_key_names_its_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("module.sock");
    std::fs::write(dir.path().join("empty"), "\n").unwrap();
    for key in [
        SecretString::Plaintext(String::new()),
        SecretString::File(dir.path().join("empty")),
        SecretString::File(dir.path().join("missing")),
    ] {
        let cfg = ServiceConfig {
            listen: ServiceListen::Uds(UdsListen { path: path.clone() }),
            key,
            soul_wait: HumanDuration(Duration::from_secs(1)),
            transport: Config::default(),
        };
        match Module::new().listen(&cfg).await {
            Err(ConnectError::Config(message)) => {
                assert!(message.starts_with("module.key:"), "{message}");
            }
            other => panic!("expected a config error, got {other:?}"),
        }
        assert!(!path.exists());
    }
}
