//! Секция `module` и пауза реконнекта.

use std::time::Duration;

use serde_json::json;

use eva_sdk::{Backoff, ModuleConfig};

#[test]
fn module_section_takes_defaults() {
    let cfg: ModuleConfig = serde_json::from_value(json!({
        "kernel": "uds:///run/eva-kernel/modules.sock",
        "token": "s3cret",
    }))
    .unwrap();
    assert_eq!(cfg.kernel.scheme(), "uds");
    assert_eq!(cfg.token.expose(), "s3cret");
    assert_eq!(*cfg.connect_wait, Duration::from_secs(180));
    assert!(cfg.tls.extra_ca.is_none());
    assert_eq!(*cfg.transport.ping_interval, Duration::from_secs(15));

    let cfg: ModuleConfig = serde_json::from_value(json!({
        "kernel": "wss://eva.example:8443/v1/modules",
        "token": "s3cret",
        "connect_wait": "10s",
        "ping_interval": "5s",
        "tls": {"extra_ca": "/etc/eva/ca.pem"},
    }))
    .unwrap();
    assert_eq!(*cfg.connect_wait, Duration::from_secs(10));
    assert_eq!(*cfg.transport.ping_interval, Duration::from_secs(5));
    assert!(cfg.tls.extra_ca.is_some());
}

#[test]
fn token_is_required() {
    let res = serde_json::from_value::<ModuleConfig>(json!({"kernel": "uds:///x"}));
    assert!(res.is_err());
}

#[test]
fn token_never_prints() {
    let cfg: ModuleConfig = serde_json::from_value(json!({
        "kernel": "uds:///x",
        "token": "s3cret",
    }))
    .unwrap();
    assert!(!format!("{cfg:?}").contains("s3cret"));
    assert!(!serde_json::to_string(&cfg).unwrap().contains("s3cret"));
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
