//! Секции конфига: дефолты глав и формы с тегами (`00 §5`, `01 §8.2`,
//! `06 §11`, `07 §9`).

use std::time::Duration;

use serde_json::json;

use eva_kernel::config::{
    ChatsConfig, DbConfig, HandsConfig, LlmConfig, ModulesConfig, NotesConfig, SystemConfig,
    WorkConfig, hands::EndpointConfig, llm::ProviderEntry,
};
use eva_protocol::types::{Effort, chat, module::TokenKind};
use eva_providers::{ProviderConfig, mock::Exhausted};

#[test]
fn empty_sections_take_chapter_defaults() {
    let db: DbConfig = serde_json::from_value(json!({})).expect("db");
    assert_eq!(*db.busy_timeout, Duration::from_secs(5));
    assert_eq!(db.readers.into_inner(), 4);
    assert_eq!(db.path, None);

    let chats: ChatsConfig = serde_json::from_value(json!({})).expect("chats");
    assert_eq!(chats.default_kind, chat::Kind::Ephemeral);
    assert_eq!(chats.persons.master_name, "Master");
    assert!(chats.persons.master.is_empty());

    let modules: ModulesConfig = serde_json::from_value(json!({})).expect("modules");
    assert_eq!(
        *modules.transport.handshake_timeout,
        Duration::from_secs(10)
    );
    assert_eq!(modules.listen.len(), 1);
    assert_eq!(*modules.hooks.timeout, Duration::from_secs(2));

    let work: WorkConfig = serde_json::from_value(json!({})).expect("work");
    assert_eq!(work.blob_max.as_u64(), 64 * 1024 * 1024);

    let notes: NotesConfig = serde_json::from_value(json!({})).expect("notes");
    assert!(notes.enabled);
    assert_eq!(
        (notes.budget_percent.into_inner(), notes.budget_max_tokens),
        (1, 4000)
    );

    let system: SystemConfig = serde_json::from_value(json!({"rt": {"flavor": "single_threaded"}}))
        .expect("framework keys of the system section are ignored here");
    assert_eq!(system.tz, None);
}

#[test]
fn llm_providers_split_module_from_builtin() {
    let llm: LlmConfig = serde_json::from_value(json!({
        "providers": {
            "scripted": {"kind": "mock", "on_exhausted": "fail"},
            "remote": {"kind": "module", "allow": ["big-model"]},
        },
        "default_model": "small-model",
    }))
    .expect("llm");

    assert!(matches!(
        &llm.providers["scripted"],
        ProviderEntry::Builtin(ProviderConfig::Mock(m)) if m.on_exhausted == Exhausted::Fail
    ));
    assert!(matches!(&llm.providers["remote"], ProviderEntry::Module(m) if m.allow.is_some()));
    assert!(llm.names_module("remote"));
    assert!(!llm.names_module("scripted"));
    assert_eq!(llm.default_effort, Effort::Medium);
    assert_eq!(llm.retries, 3);
    assert_eq!(llm.currency, "USD");

    let missing = serde_json::from_value::<LlmConfig>(json!({"providers": {}}));
    assert!(missing.is_err(), "default_model is required");

    let unknown = serde_json::from_value::<LlmConfig>(json!({
        "providers": {"x": {"kind": "acme"}},
        "default_model": "m",
    }))
    .expect_err("an unknown kind is refused");
    assert!(
        unknown.to_string().contains("acme"),
        "names the kind: {unknown}"
    );
}

#[test]
fn hands_endpoints_are_local_or_remote() {
    let hands: HandsConfig = serde_json::from_value(json!({
        "endpoints": {
            "here": {"local": true, "limits": {"exec": false, "exec_timeout": "30s", "roots": ["/srv/work"]}},
            "laptop": {"address": "10.0.0.2:7777", "key": "secret"},
        },
        "default_endpoint": "here",
    }))
    .expect("hands");

    let EndpointConfig::Local { local, limits } = &hands.endpoints["here"] else {
        panic!("here is local");
    };
    assert!(*local);
    assert!(!limits.exec);
    assert_eq!(*limits.exec_timeout, Duration::from_secs(30));
    let limits = eva_hands::Limits::from(limits.clone());
    assert_eq!(limits.exec_timeout_secs, 30);
    assert!(matches!(
        &hands.endpoints["laptop"],
        EndpointConfig::Remote { .. }
    ));
}

#[test]
fn module_tokens_never_serialize_their_secret() {
    let modules: ModulesConfig = serde_json::from_value(json!({
        "tokens": {"phone": {"token": "hunter2", "kind": "client"}},
    }))
    .expect("modules");
    let entry = &modules.tokens[&eva_protocol::types::ModuleId::from("phone")];
    assert_eq!(entry.kind, TokenKind::Client);
    assert_eq!(entry.token.expose(), "hunter2");

    let dumped = serde_json::to_string(&modules).expect("serialize");
    assert!(!dumped.contains("hunter2"));
    assert!(!format!("{modules:?}").contains("hunter2"));
}
