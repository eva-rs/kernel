//! Секция `module` и пауза реконнекта.

use std::time::Duration;

use serde_json::json;

use eva::url::Url;
use eva_rpc::{Config, Conn};
use eva_sdk::{Backoff, ConnectError, Module, ModuleConfig, SecretString};

use crate::fake::{self, Surfaces};

#[test]
fn module_section_takes_defaults() {
    let cfg: ModuleConfig = serde_json::from_value(json!({
        "kernel": "uds:///run/eva-kernel/modules.sock",
        "token": {"plaintext": "s3cret"},
    }))
    .unwrap();
    assert_eq!(cfg.kernel.scheme(), "uds");
    assert_eq!(cfg.token.read().unwrap(), "s3cret");
    assert_eq!(*cfg.connect_wait, Duration::from_secs(180));
    assert!(cfg.tls.extra_ca.is_none());
    assert_eq!(*cfg.transport.ping_interval, Duration::from_secs(15));

    let cfg: ModuleConfig = serde_json::from_value(json!({
        "kernel": "wss://eva.example:8443/v1/modules",
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
    let res = serde_json::from_value::<ModuleConfig>(json!({"kernel": "uds:///x"}));
    assert!(res.is_err());
    let bare = serde_json::from_value::<ModuleConfig>(json!({
        "kernel": "uds:///x",
        "token": "s3cret",
    }));
    assert!(bare.is_err(), "a secret names its form: plaintext or file");
}

#[test]
fn token_never_prints() {
    let cfg: ModuleConfig = serde_json::from_value(json!({
        "kernel": "uds:///x",
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

    let (module_end, kernel_end) = Conn::pair(&Config::default());
    let kernel = tokio::spawn(fake::accept(
        kernel_end,
        fake::welcome("s1", &[]),
        Surfaces::default(),
    ));
    let _connected = Module::new()
        .handshake(module_end, &SecretString::File(path))
        .await
        .expect("handshake");
    let session = kernel.await.unwrap();
    assert_eq!(session.hello.token.expose(), "s3cret");
}

/// Нечитаемый секрет — ошибка старта с ключом конфига, до сети дело не доходит.
#[tokio::test]
async fn an_unreadable_token_names_its_key() {
    let dir = tempfile::tempdir().unwrap();
    let cfg: ModuleConfig = serde_json::from_value(json!({
        "kernel": format!("uds://{}", dir.path().join("none.sock").display()),
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
