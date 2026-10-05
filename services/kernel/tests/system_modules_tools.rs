mod common;
mod turns_support;

use common::Harness;
use eva_kernel::{
    config::{Config, chats::MasterIdentity},
    db::{Db as _, Queries as _},
    subsystems::{
        modules::tools,
        turns::{
            ctx::{self, Entry},
            ports::ToolsetCall,
        },
    },
};
use eva_protocol::{
    types::{ChatId, ModuleId, PersonId, SurfaceId, TurnId, tool::ToolContent, turn::ActorKind},
    wire::tools::ToolResult,
};
use serde_json::{Value, json};

fn configure(cfg: &mut Config) {
    cfg.chats.persons.master = vec![MasterIdentity {
        surface: SurfaceId::from("local"),
        external_id: "module-tools-human".into(),
        module: None,
    }];
}

async fn human(h: &Harness) -> PersonId {
    h.query(|c| {
        Ok(PersonId(c.query_row(
            "SELECT id FROM persons WHERE master = 1 AND id <> 'p0' LIMIT 1",
            [],
            |row| row.get(0),
        )?))
    })
    .await
}

async fn invoke(
    h: &Harness,
    chat: ChatId,
    name: &str,
    input: Value,
    master: bool,
    trusted: bool,
    actor: Option<PersonId>,
) -> ToolResult {
    let entry = Entry::master(chat, actor.clone().unwrap_or_else(PersonId::system));
    let cfg = h.kernel.shared().cfg().llm.clone();
    let mut ctx = h
        .kernel
        .shared()
        .db()
        .reader()
        .await
        .unwrap()
        .run(move |c| ctx::resolve(c, entry, &cfg))
        .await
        .unwrap();
    ctx.master_turn = master;
    ctx.trusted_chat = trusted;
    ctx.actor.kind = if actor.is_some() {
        ActorKind::Master
    } else {
        ActorKind::Unknown
    };
    ctx.actor.person_id = actor;
    tools::call(
        h.kernel.shared(),
        ToolsetCall {
            canonical: name,
            call_id: "module-tools-test",
            input,
            ctx: &ctx.wire(&TurnId::from("module-tools-test")),
            view: &ctx.view(),
            hot_ids: &[],
            ratio: None,
        },
    )
    .await
    .unwrap()
}

fn text(result: ToolResult) -> String {
    assert!(!result.is_error, "{result:?}");
    let ToolContent::Text { text } = &result.content[0] else {
        panic!("text result")
    };
    text.clone()
}

async fn live(h: &Harness, token: &str) -> Option<ModuleId> {
    let hash = blake3::hash(token.as_bytes());
    h.query(move |c| c.modules().token(hash.as_bytes())).await
}

#[tokio::test]
async fn issue_records_the_human_actor_and_tokens_can_be_rotated_and_revoked() {
    let h = common::start_with(configure).await;
    let chat = turns_support::chat(&h).await;
    let actor = human(&h).await;
    let output = text(
        invoke(
            &h,
            chat,
            "system_modules_token_issue",
            json!({"module":"tablet"}),
            true,
            true,
            Some(actor.clone()),
        )
        .await,
    );
    let first = output.lines().last().unwrap().to_owned();
    assert_eq!(first.len(), 64);
    assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    let (scrubbed, hits) = h.kernel.shared().turns().scrubber().scrub(&output);
    assert_eq!(
        hits, 0,
        "intentional token presentation survives the default scrubber"
    );
    assert_eq!(scrubbed.as_ref(), output);
    type Row = (Vec<u8>, Option<String>, Option<String>);
    let rows: Vec<Row> = h.query(|c| {
        let mut query = c.prepare("SELECT token_hash, issued_by, issued_via FROM module_tokens WHERE module = 'tablet'")?;
        Ok(query.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?)
    }).await;
    assert_eq!(
        rows,
        vec![(
            blake3::hash(first.as_bytes()).as_bytes().to_vec(),
            Some(actor.to_string()),
            None
        )]
    );
    assert_eq!(live(&h, &first).await, Some(ModuleId::from("tablet")));

    let output = text(
        invoke(
            &h,
            chat,
            "system_modules_token_issue",
            json!({"module":"tablet"}),
            true,
            true,
            Some(actor.clone()),
        )
        .await,
    );
    let second = output.lines().last().unwrap();
    assert_ne!(first, second);
    assert_eq!(live(&h, &first).await, None);
    assert_eq!(live(&h, second).await, Some(ModuleId::from("tablet")));
    assert_eq!(
        text(
            invoke(
                &h,
                chat,
                "system_modules_token_revoke",
                json!({"module":"tablet"}),
                true,
                false,
                Some(actor.clone())
            )
            .await
        ),
        "{}"
    );
    assert_eq!(live(&h, second).await, None);
    assert!(
        invoke(
            &h,
            chat,
            "system_modules_token_revoke",
            json!({"module":"tablet"}),
            true,
            true,
            Some(actor)
        )
        .await
        .is_error
    );
}

#[tokio::test]
async fn public_unknown_and_system_turns_cannot_issue_or_forge_token_authorship() {
    let h = common::start_with(|cfg| {
        configure(cfg);
        common::with_modules(cfg);
    })
    .await;
    let chat = turns_support::chat(&h).await;
    let actor = human(&h).await;
    for (master, trusted, sender) in [
        (true, false, Some(actor.clone())),
        (false, true, Some(actor.clone())),
        (true, true, None),
        (true, true, Some(PersonId::system())),
    ] {
        assert!(
            invoke(
                &h,
                chat,
                "system_modules_token_issue",
                json!({"module":"tablet"}),
                master,
                trusted,
                sender
            )
            .await
            .is_error
        );
    }
    for input in [
        json!({"module":"tablet", "issued_by":"p0"}),
        json!({"module":"tablet", "view":{"master_turn":true}}),
        json!({"module":"kernel"}),
        json!({"module":common::CLIENT}),
    ] {
        assert!(
            invoke(
                &h,
                chat,
                "system_modules_token_issue",
                input,
                true,
                true,
                Some(actor.clone())
            )
            .await
            .is_error
        );
    }
    let count: i64 = h
        .query(|c| Ok(c.query_row("SELECT COUNT(*) FROM module_tokens", [], |r| r.get(0))?))
        .await;
    assert_eq!(count, 0, "refused requests must not create tokens");
}

#[tokio::test]
async fn module_listing_has_the_specified_columns_and_master_only_gates() {
    let h = common::start_with(configure).await;
    h.register_module(common::CLIENT).await;
    let chat = turns_support::chat(&h).await;
    let actor = human(&h).await;
    let listed = text(
        invoke(
            &h,
            chat,
            "system_modules_list",
            json!({}),
            true,
            false,
            Some(actor.clone()),
        )
        .await,
    );
    assert!(
        listed.starts_with(
            "name | kind | state | version | since | capabilities | address | error\n"
        )
    );
    assert!(listed.contains(common::CLIENT));
    for name in ["system_modules_list", "system_modules_token_revoke"] {
        let input = if name.ends_with("list") {
            json!({})
        } else {
            json!({"module":"tablet"})
        };
        assert!(
            invoke(&h, chat, name, input, false, true, Some(actor.clone()))
                .await
                .is_error
        );
    }
    let decls = tools::decls();
    assert_eq!(decls.len(), 3);
    for decl in decls {
        assert!(decl.master_only && !decl.ends_turn && decl.needs.is_empty());
        assert_eq!(decl.trusted_only, decl.path == "system.modules.token_issue");
        assert!(decl.input.get("title").is_none());
        assert!(decl.input["properties"].get("issued_by").is_none());
    }
}
