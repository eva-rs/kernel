//! Секреты конфига ядра (`00 §5`): `eva::utils::SecretString` в формах
//! `{plaintext}` и `{file}`, читаются один раз на старте, хвостовой перевод
//! строки срезается, ошибка чтения называет ключ; `Debug`/`Display` секрет
//! не выдают. Здесь же — отказы старта по записям `modules.tokens` и
//! `modules.services` (`06 §2`).

mod common;

use std::{collections::BTreeMap, path::Path};

use serde_json::json;

use eva::utils::SecretString;
use eva_hands::Psk;
use eva_kernel::{
    Kernel, Real,
    config::{
        HandsConfig, ModulesConfig,
        hands::EndpointConfig,
        modules::{ServiceEntry, TokenEntry},
    },
    hands,
    subsystems::modules::{Registrations, tokens::ConfigTokens},
};
use eva_protocol::types::{ModuleId, module::ServiceAddress};

const CREDENTIALS: &str = "/run/credentials/eva-kernel.service";
const KEY: &str = "bridge-key-0123456789abcdef0123456789";

fn entry(token: SecretString) -> TokenEntry {
    TokenEntry::new(token)
}

fn service(key: SecretString) -> ServiceEntry {
    ServiceEntry {
        address: ServiceAddress::Uds("/run/eva-bridge/module.sock".into()),
        key,
    }
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

fn registration_error(tweak: impl FnOnce(&mut ModulesConfig)) -> String {
    let mut cfg = ModulesConfig::default();
    tweak(&mut cfg);
    format!(
        "{:#}",
        Registrations::load(&cfg)
            .err()
            .expect("the start is refused")
    )
}

#[test]
fn every_secret_takes_the_plaintext_and_the_file_form() {
    let modules: ModulesConfig = serde_json::from_value(json!({
        "tokens": {
            "phone": {"token": {"plaintext": "hunter2"}},
            "tablet": {"token": {"file": format!("{CREDENTIALS}/tablet")}},
        },
        "services": {
            "bridge": {"address": "uds:///run/eva-bridge/module.sock", "key": {"plaintext": "hunter4"}},
            "mcp": {"address": "tcp://10.0.0.4:7000", "key": {"file": format!("{CREDENTIALS}/mcp")}},
        },
    }))
    .expect("modules");
    assert!(matches!(
        &modules.tokens[&ModuleId::from("phone")].token,
        SecretString::Plaintext(t) if t == "hunter2"
    ));
    assert!(matches!(
        &modules.tokens[&ModuleId::from("tablet")].token,
        SecretString::File(p) if p.as_path() == Path::new(&format!("{CREDENTIALS}/tablet"))
    ));
    assert!(matches!(
        &modules.services[&ModuleId::from("bridge")].key,
        SecretString::Plaintext(k) if k == "hunter4"
    ));
    assert!(matches!(
        &modules.services[&ModuleId::from("mcp")].key,
        SecretString::File(p) if p.as_path() == Path::new(&format!("{CREDENTIALS}/mcp"))
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
        "tokens": {"phone": {"token": "hunter2"}},
    }));
    assert!(bare.is_err(), "a bare string is neither form");

    let shown = format!(
        "{modules:?} {hands:?} {} {}",
        modules.tokens[&ModuleId::from("phone")].token,
        modules.services[&ModuleId::from("bridge")].key,
    );
    assert!(
        !shown.contains("hunter2") && !shown.contains("hunter3") && !shown.contains("hunter4"),
        "{shown}"
    );
}

#[test]
fn a_credential_file_with_a_trailing_newline_is_the_bare_secret() {
    let dir = tempfile::tempdir().expect("temp dir");
    let secret = "tablet-token-0123456789abcdef0123456789";
    let loaded = ConfigTokens::load(&tokens(&[(
        "tablet",
        entry(credential(&dir, "tablet", secret)),
    )]))
    .expect("tokens load");
    assert_eq!(
        loaded.find(&blake3::hash(secret.as_bytes())),
        Some(ModuleId::from("tablet"))
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

    let error = load_error(&[("tablet", entry(missing.clone()))]);
    assert!(error.contains("modules.tokens.tablet.token"), "{error}");
    let error = format!(
        "{:#}",
        hands::endpoint_key("laptop", &missing).expect_err("missing key file")
    );
    assert!(error.contains("hands.endpoints.laptop.key"), "{error}");

    let error = load_error(&[("tablet", entry(plaintext("\n")))]);
    assert!(error.contains("modules.tokens.tablet.token"), "{error}");

    let error = registration_error(|cfg| {
        cfg.services
            .insert(ModuleId::from("bridge"), service(missing.clone()));
    });
    assert!(error.contains("modules.services.bridge.key"), "{error}");
    let error = registration_error(|cfg| {
        cfg.services
            .insert(ModuleId::from("bridge"), service(plaintext("\n")));
    });
    assert!(
        error.contains("modules.services.bridge.key") && error.contains("empty"),
        "{error}"
    );
}

#[test]
fn reserved_ids_and_shared_tokens_fail_the_start() {
    for reserved in ["kernel", "system", "local"] {
        let error = load_error(&[(reserved, entry(plaintext("tok")))]);
        assert!(
            error.contains(&format!("modules.tokens.{reserved}")) && error.contains("reserved"),
            "{error}"
        );
    }

    let dir = tempfile::tempdir().expect("temp dir");
    let error = load_error(&[
        ("phone", entry(plaintext("same"))),
        ("tablet", entry(credential(&dir, "tablet", "same"))),
    ]);
    assert!(
        error.contains("modules.tokens.tablet") && error.contains("modules.tokens.phone"),
        "one token cannot name two modules: {error}"
    );
}

/// Сервис с токеном — ошибка старта, а не молча клиент (`06 §11`).
#[test]
fn a_token_kind_fails_the_start_pointing_to_services() {
    let modules: ModulesConfig = serde_json::from_value(json!({
        "tokens": {"bridge": {"token": {"plaintext": "bridge-token"}, "kind": "service"}},
    }))
    .expect("the old form still parses");
    let error = format!(
        "{:#}",
        Registrations::load(&modules)
            .err()
            .expect("the start is refused")
    );
    assert!(
        error.contains("modules.tokens.bridge.kind") && error.contains("modules.services.bridge"),
        "{error}"
    );
}

#[test]
fn a_service_id_is_neither_reserved_nor_a_client() {
    for reserved in ["kernel", "system", "local"] {
        let error = registration_error(|cfg| {
            cfg.services
                .insert(ModuleId::from(reserved), service(plaintext(KEY)));
        });
        assert!(
            error.contains(&format!("modules.services.{reserved}")) && error.contains("reserved"),
            "{error}"
        );
    }
    let error = registration_error(|cfg| {
        cfg.services
            .insert(ModuleId::from("Bridge"), service(plaintext(KEY)));
    });
    assert!(error.contains("modules.services.Bridge"), "{error}");

    let error = registration_error(|cfg| {
        cfg.services
            .insert(ModuleId::from("bridge"), service(plaintext(KEY)));
        cfg.tokens
            .insert(ModuleId::from("bridge"), entry(plaintext("bridge-token")));
    });
    assert!(
        error.contains("modules.services.bridge") && error.contains("modules.tokens.bridge"),
        "one id is a service or a client: {error}"
    );

    let short = ModulesConfig {
        services: BTreeMap::from([(ModuleId::from("bridge"), service(plaintext("short")))]),
        ..ModulesConfig::default()
    };
    Registrations::load(&short).expect("a short key only warns");
}

/// Кривой `address` — ошибка разбора конфига, то есть старта.
#[test]
fn a_bad_service_address_fails_the_config() {
    for bad in [
        "uds://relative.sock",
        "tcp://bridge.lan",
        "tcp://bridge.lan:7000/path",
        "http://bridge.lan:7000",
    ] {
        let error = serde_json::from_value::<ModulesConfig>(json!({
            "services": {"bridge": {"address": bad, "key": {"plaintext": KEY}}},
        }))
        .expect_err(bad);
        assert!(
            error.to_string().contains(bad),
            "names the address: {error}"
        );
    }
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
    cfg.modules
        .tokens
        .insert(ModuleId::from("tablet"), entry(missing.clone()));
    let error = refused(cfg).await;
    assert!(error.contains("modules.tokens.tablet.token"), "{error}");

    let mut cfg = common::config(dir.path());
    cfg.modules
        .services
        .insert(ModuleId::from("bridge"), service(missing));
    let error = refused(cfg).await;
    assert!(error.contains("modules.services.bridge.key"), "{error}");
}
