//! Проводная форма: roundtrip операций, `Patch` и различение `Aux`/`Spec`.

use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use eva_protocol::{
    errors::{self, Aux, Generic},
    requests::{self, Chats, Patch, Selector},
    types::{
        ChatId, EventId, TurnId,
        content::Block,
        generate::{GenItem, Stop, StopReason},
        turn::{FeedEvent, TurnItem, TurnOutcome},
    },
};

fn roundtrip<T>(value: &T) -> Value
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_value(value).unwrap();
    let back: T = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(&back, value);
    json
}

fn parse<T: DeserializeOwned>(json: Value) -> T {
    match serde_json::from_value(json) {
        Ok(value) => value,
        Err(e) => panic!("{}: {e}", std::any::type_name::<T>()),
    }
}

#[test]
fn args_ok_err_roundtrip() {
    let json = roundtrip(&Chats::delete::Args {
        chat: ChatId(42),
        purge: true,
    });
    assert_eq!(json, json!({"chat": 42, "purge": true}));

    let args: Chats::delete::Args = parse(json!({"chat": 7}));
    assert!(!args.purge);

    roundtrip(&Chats::delete::Ok {});
    roundtrip(&Chats::delete::Err::from(errors::chats::Undeletable {
        chat: ChatId(1),
    }));

    let by_id = roundtrip(&Chats::get::Args {
        chat: ChatId(42).into(),
    });
    assert_eq!(by_id, json!({"chat": 42}));
    let by_alias = roundtrip(&Chats::get::Args {
        chat: Selector::Alias {
            surface: "local".into(),
            external_id: "x".into(),
        },
    });
    assert_eq!(
        by_alias,
        json!({"chat": {"surface": "local", "external_id": "x"}})
    );

    let ingest = roundtrip(&Chats::ingest::Args {
        surface: "local".into(),
        external_chat: Chats::ingest::ExternalChat {
            id: "1".into(),
            kind: eva_protocol::types::chat::ExternalKind::Direct,
            title: None,
        },
        sender: None,
        content: vec![Block::text("hi")],
        external_id: None,
        reply_to_external_id: None,
        anchor: Some(json!([1, 2])),
        r#override: Some(Chats::ingest::Override::default()),
        silent: false,
    });
    assert!(ingest.get("override").is_some(), "{ingest}");

    let item = roundtrip(&TurnItem {
        seq: EventId(9),
        sub: 0,
        replayed: true,
        event: FeedEvent::TurnDone {
            turn: TurnId::from("t"),
            outcome: TurnOutcome::Failed,
            error: Some("boom".into()),
        },
    });
    assert_eq!(item["event"]["turn_done"]["outcome"], json!("failed"));

    roundtrip(&GenItem::Stop(Stop {
        reason: StopReason::Refusal,
        details: None,
    }));
}

#[test]
fn patch_defaults() {
    let upd: Chats::update::Update = parse(json!({}));
    assert_eq!(upd, Chats::update::Update::default());

    parse::<requests::Surfaces::update::Update>(json!({}));
    parse::<requests::Folders::update::Update>(json!({}));
    parse::<Chats::update::Args>(json!({"chat": 1}));

    let upd: Chats::update::Update = parse(json!({"title": null, "pinned": "keep"}));
    assert!(upd.title.is_keep());
    assert!(upd.pinned.is_keep());

    let upd: Chats::update::Update =
        parse(json!({"title": {"change": null}, "pinned": {"change": true}}));
    assert_eq!(upd.title, Patch::Change(None));
    assert_eq!(upd.pinned, Patch::Change(true));
}

#[test]
fn generic_untagged() {
    type E = Generic<Chats::delete::Err>;

    let aux = E::Aux(Aux::Forbidden("owner only".into()));
    let json = roundtrip(&aux);
    assert_eq!(json, json!({"forbidden": "owner only"}));

    let spec = E::Spec(errors::chats::Undeletable { chat: ChatId(5) }.into());
    let json = roundtrip(&spec);
    assert_eq!(json, json!({"undeletable": {"chat": 5}}));

    assert!(matches!(parse::<E>(json), Generic::Spec(_)));
    assert!(matches!(
        parse::<E>(json!({"not_found": "folder"})),
        Generic::Aux(Aux::NotFound(_))
    ));
}

#[test]
fn schema_uses_wire_names() {
    let schema = eva::_priv::schemars::schema_for!(Chats::ingest::Args);
    let json = serde_json::to_value(&schema).unwrap();
    assert!(json["properties"].get("override").is_some(), "{json}");
}
