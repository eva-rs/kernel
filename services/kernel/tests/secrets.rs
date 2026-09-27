//! Секреты конфига ядра (`00 §5`): `eva::utils::SecretString` в формах
//! `{plaintext}` и `{file}`, читаются один раз на старте, хвостовой перевод
//! строки срезается, ошибка чтения называет ключ; `Debug`/`Display` секрет
//! не выдают.

mod common;

use std::{collections::BTreeMap, path::Path};

use serde_json::json;

use eva::utils::SecretString;
use eva_hands::Psk;
use eva_kernel::{
    Kernel, Real,
    config::{HandsConfig, ModulesConfig, hands::EndpointConfig, modules::TokenEntry},
    hands,
    subsystems::modules::tokens::ConfigTokens,
};
use eva_protocol::types::{ModuleId, module::TokenKind};

const CREDENTIALS: &str = "/run/credentials/eva-kernel.service";

fn entry(token: SecretString, kind: TokenKind) -> TokenEntry {
    TokenEntry { token, kind }
}

fn plaintext(secret: &str) -> SecretString {
    SecretString::Plaintext(secret.to_owned())
}

/// Файл секрета, как его кладёт `LoadCredential`: с переводом строки в конце.
fn credential(dir: &tempfile::TempDir, name: &str, secret: &str) -> SecretString {
    let path = dir.path().join(name);
    std::fs::write(&path, format!("{secret}\n")).expect("credential file");
    SecretString::File(path)
}

fn tokens(entries: &[(&str, TokenEntry)]) -> BTreeMap<ModuleId, TokenEntry> {
    entries
        .iter()
        .map(|(module, entry)| (ModuleId::from(*module), entry.clone()))
        .collect()
}

fn load_error(entries: &[(&str, TokenEntry)]) -> String {
    format!(
        "{:#}",
        ConfigTokens::load(&tokens(entries)).expect_err("the start is refused")
    )
}

#[test]
fn every_secret_takes_the_plaintext_and_the_file_form() {
    let modules: ModulesConfig = serde_json::from_value(json!({
        "tokens": {
            "phone": {"token": {"plaintext": "hunter2"}, "kind": "client"},
            "bridge": {"token": {"file": format!("{CREDENTIALS}/bridge")}, "kind": "service"},
        },
    }))
    .expect("modules");
    assert!(matches!(
        &modules.tokens[&ModuleId::from("phone")].token,
        SecretString::Plaintext(t) if t == "hunter2"
    ));
    assert!(matches!(
        &modules.tokens[&ModuleId::from("bridge")].token,
        SecretString::File(p) if p.as_path() == Path::new(&format!("{CREDENTIALS}/bridge"))
    ));

    let hands: HandsConfig = serde_json::from_value(json!({
        "endpoints": {
            "laptop": {"address": "10.0.0.2:7777", "key": {"plaintext": "hunter3"}},
            "desk": {"address": "10.0.0.3:7777", "key": {"file": format!("{CREDENTIALS}/desk")}},
        },
    }))
    .expect("hands");
    assert!(matches!(
        &hands.endpoints["laptop"],
        EndpointConfig::Remote { key: SecretString::Plaintext(k), .. } if k == "hunter3"
    ));
    assert!(matches!(
        &hands.endpoints["desk"],
        EndpointConfig::Remote {
            key: SecretString::File(_),
            ..
        }
    ));

    let bare = serde_json::from_value::<ModulesConfig>(json!({
        "tokens": {"phone": {"token": "hunter2", "kind": "client"}},
    }));
    assert!(bare.is_err(), "a bare string is neither form");

    let shown = format!(
        "{modules:?} {hands:?} {}",
        modules.tokens[&ModuleId::from("phone")].token
    );
    assert!(
        !shown.contains("hunter2") && !shown.contains("hunter3"),
        "{shown}"
    );
}

#[test]
fn a_credential_file_with_a_trailing_newline_is_the_bare_secret() {
    let dir = tempfile::tempdir().expect("temp dir");
    let secret = "bridge-token-0123456789abcdef0123456789";
    let loaded = ConfigTokens::load(&tokens(&[(
        "bridge",
        entry(credential(&dir, "bridge", secret), TokenKind::Service),
    )]))
    .expect("tokens load");
    assert_eq!(
        loaded.find(&blake3::hash(secret.as_bytes())),
        Some((ModuleId::from("bridge"), TokenKind::Service))
    );
    assert_eq!(
        loaded.find(&blake3::hash(format!("{secret}\n").as_bytes())),
        None
    );

    let key = hands::endpoint_key("laptop", &credential(&dir, "laptop", "k3y")).expect("psk");
    assert_eq!(key, Psk::new("k3y"));
    assert_ne!(key, Psk::new("k3y\n"));
}

#[test]
fn an_unreadable_secret_fails_the_start_naming_its_key() {
    let dir = tempfile::tempdir().expect("temp dir");
    let missing = SecretString::File(dir.path().join("missing"));

    let error = load_error(&[("bridge", entry(missing.clone(), TokenKind::Service))]);
    assert!(error.contains("modules.tokens.bridge.token"), "{error}");
    let error = format!(
        "{:#}",
        hands::endpoint_key("laptop", &missing).expect_err("missing key file")
    );
    assert!(error.contains("hands.endpoints.laptop.key"), "{error}");

    let error = load_error(&[("bridge", entry(plaintext("\n"), TokenKind::Service))]);
    assert!(error.contains("modules.tokens.bridge.token"), "{error}");
}

#[test]
fn reserved_ids_and_shared_tokens_fail_the_start() {
    for reserved in ["kernel", "system", "local"] {
        let error = load_error(&[(reserved, entry(plaintext("tok"), TokenKind::Client))]);
        assert!(
            error.contains(&format!("modules.tokens.{reserved}")) && error.contains("reserved"),
            "{error}"
        );
    }

    let dir = tempfile::tempdir().expect("temp dir");
    let error = load_error(&[
        ("phone", entry(plaintext("same"), TokenKind::Client)),
        (
            "bridge",
            entry(credential(&dir, "bridge", "same"), TokenKind::Service),
        ),
    ]);
    assert!(
        error.contains("modules.tokens.bridge") && error.contains("modules.tokens.phone"),
        "one token cannot name two modules: {error}"
    );
}

async fn refused(cfg: eva_kernel::Config) -> String {
    format!(
        "{:#}",
        Kernel::<Real>::from_config(cfg)
            .await
            .err()
            .expect("start refused")
    )
}

/// Боевой старт: ошибка чтения любого секрета — отказ старта с путём ключа.
#[tokio::test]
async fn the_kernel_refuses_to_start_on_an_unreadable_secret() {
    let dir = tempfile::tempdir().expect("temp dir");
    let missing = SecretString::File(dir.path().join("missing"));

    let mut cfg = common::config(dir.path());
    cfg.hands.endpoints.insert(
        String::from("laptop"),
        EndpointConfig::Remote {
            address: String::from("127.0.0.1:1"),
            key: missing.clone(),
        },
    );
    let error = refused(cfg).await;
    assert!(error.contains("hands.endpoints.laptop.key"), "{error}");

    let mut cfg = common::config(dir.path());
    cfg.modules.tokens.insert(
        ModuleId::from("bridge"),
        entry(missing.clone(), TokenKind::Service),
    );
    let error = refused(cfg).await;
    assert!(error.contains("modules.tokens.bridge.token"), "{error}");
}
