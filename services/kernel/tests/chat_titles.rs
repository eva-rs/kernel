mod common;
mod turns_support;

use common::Harness;
use eva_kernel::{
    config::{Config, chats::MasterIdentity},
    db::{Db as _, DbResult, Queries as _},
    subsystems::turns::{
        ctx::Entry,
        ingest::{self, Message},
    },
};
use eva_protocol::types::{
    ChatId, ModelId, PersonId, SurfaceId,
    content::Block,
    generate::{Block as GenBlock, CacheMode, Message as GenMessage, Purpose, StopReason, Usage},
    model::Price,
};
use eva_providers::mock::Script;
use std::time::Duration;

fn configure(cfg: &mut Config) {
    cfg.context.count_prefix = false;
    cfg.chats.persons.master = vec![MasterIdentity {
        surface: SurfaceId::from("local"),
        external_id: "title-test-human".into(),
        module: None,
    }];
}

async fn unnamed(h: &Harness, surface: &'static str, folder: &'static str) -> ChatId {
    h.kernel.shared().db().write(move |tx| {
        DbResult::Ok(ChatId(tx.query_row(
            "INSERT INTO chats (folder_id, kind, trusted, created_at, last_activity_at)
             SELECT id, 'persistent', 1, 1, 1 FROM folders WHERE surface_id = ?1 AND name = ?2 RETURNING id",
            (surface, folder), |row| row.get(0),
        )?))
    }).await.expect("unnamed chat")
}

async fn append(h: &Harness, chat: ChatId, text: &str, machine: bool) {
    let person = h
        .query(|c| {
            Ok(PersonId(c.query_row(
                "SELECT id FROM persons WHERE master = 1 AND id <> 'p0' LIMIT 1",
                [],
                |row| row.get(0),
            )?))
        })
        .await;
    ingest::append(
        h.kernel.shared(),
        Entry::master(chat, person),
        Message {
            content: vec![Block::text(text)],
            external_id: None,
            reply_to_external_id: None,
            silent: true,
            machine,
        },
    )
    .await
    .expect("append");
}

async fn title(h: &Harness, chat: ChatId) -> Option<String> {
    h.kernel
        .shared()
        .db()
        .reader()
        .await
        .expect("reader")
        .run(move |c| DbResult::Ok(c.chats().get(chat)?.expect("chat").title))
        .await
        .expect("title")
}

async fn finished(h: &Harness, count: i64) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let total: i64 = h.kernel.shared().db().reader().await.expect("reader").run(|c| {
                DbResult::Ok(c.query_row("SELECT COALESCE(SUM(count), 0) FROM health_stats WHERE metric = 'chats.title'", [], |row| row.get(0))?)
            }).await.expect("count");
            if total >= count { return; }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }).await.expect("title finished");
}

#[tokio::test]
async fn parallel_ingress_generates_once_with_budgets_and_title_spend() {
    let h = common::start_with(configure).await;
    let chat = unnamed(&h, "local", "Unsorted").await;
    let (script, gate) = Script::new().gate();
    h.provider.push(
        script
            .text("Title: «Balcony tomatoes.»\nignored")
            .usage(Usage {
                input: 21,
                output: 5,
                ..Default::default()
            })
            .stop(StopReason::EndTurn),
    );
    let input = "First line ".repeat(400);
    append(&h, chat, &input, false).await;
    turns_support::seen(&h, 1).await;
    futures::future::join_all((0..8).map(|_| append(&h, chat, "second message", false))).await;
    assert_eq!(h.provider.requests().len(), 1);
    gate.open();
    finished(&h, 1).await;
    assert_eq!(title(&h, chat).await.as_deref(), Some("Balcony tomatoes"));
    let requests = h.provider.requests();
    let req = &requests[0];
    assert_eq!(req.purpose, Purpose::Title);
    assert_eq!(req.effort, Some(eva_protocol::types::Effort::Low));
    assert_eq!(req.cache, CacheMode::Off);
    assert!(req.breakpoints.is_empty() && req.tools.is_empty());
    assert_eq!(req.max_tokens, 528);
    let GenMessage::User { blocks } = &req.messages[0] else {
        panic!("user message");
    };
    let GenBlock::Text { text } = &blocks[0] else {
        panic!("text input");
    };
    assert!(text.len() <= 512 * 4 && input.starts_with(text));
    let spend: (String, String, i64) = h
        .kernel
        .shared()
        .db()
        .reader()
        .await
        .expect("reader")
        .run(move |c| {
            DbResult::Ok(c.query_row(
                "SELECT op, source, chat_id FROM spend WHERE op = 'title'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?)
        })
        .await
        .expect("title spend");
    assert_eq!(spend, ("title".into(), "system".into(), chat.0));
    append(&h, chat, "third message", false).await;
    assert_eq!(h.provider.requests().len(), 1, "never regenerate");
    h.kernel.shared().titles().shutdown().await;
}

#[tokio::test]
async fn manual_rename_wins_inflight_title_and_external_title_stays_authoritative() {
    let h = common::start_with(configure).await;
    let manual = unnamed(&h, "local", "Unsorted").await;
    let external = unnamed(&h, "local", "Unsorted").await;
    let (script, gate) = Script::new().gate();
    h.provider
        .push(script.text("Generated").stop(StopReason::EndTurn));
    append(&h, manual, "first topic", false).await;
    turns_support::seen(&h, 1).await;
    h.kernel
        .shared()
        .db()
        .write(move |tx| {
            let mut row = tx.chats().get(manual)?.expect("chat");
            row.title = Some("Manual rename".into());
            tx.chats().save(&row)?;
            tx.chats()
                .set_external_title(external, Some("Surface title"))
        })
        .await
        .expect("rename");
    append(&h, external, "second topic", false).await;
    gate.open();
    finished(&h, 2).await;
    assert_eq!(title(&h, manual).await.as_deref(), Some("Manual rename"));
    assert_eq!(title(&h, external).await, None);
    assert_eq!(h.provider.requests().len(), 1);
    h.kernel.shared().titles().shutdown().await;
}

#[tokio::test]
async fn truncated_model_output_falls_back_to_first_line_with_unicode_budget() {
    let h = common::start_with(|cfg| {
        configure(cfg);
        cfg.chats.title.len_tokens = 4;
    })
    .await;
    let chat = unnamed(&h, "local", "Unsorted").await;
    h.provider.push(
        Script::new()
            .text("A misleading partial title")
            .stop(StopReason::MaxTokens),
    );
    append(&h, chat, "Очень длинная строка\nnot in title", false).await;
    finished(&h, 1).await;
    let title = title(&h, chat).await.expect("fallback");
    assert!(title.ends_with('…') && title.len() <= 16);
    assert!("Очень длинная строка".starts_with(title.trim_end_matches('…')));
    assert!(!title.contains("not in title"));
    h.kernel.shared().titles().shutdown().await;
}

#[tokio::test]
async fn offline_title_model_uses_fallback_without_inference() {
    let h = common::start_with(|cfg| {
        configure(cfg);
        cfg.llm.roles.title = Some(ModelId::from("offline"));
    })
    .await;
    let chat = unnamed(&h, "local", "Unsorted").await;
    append(&h, chat, "Fallback topic\nMore details", false).await;
    finished(&h, 1).await;
    assert_eq!(title(&h, chat).await.as_deref(), Some("Fallback topic"));
    assert!(h.provider.requests().is_empty());
    h.kernel.shared().titles().shutdown().await;
}

#[tokio::test]
async fn machine_messages_do_not_consume_first_human_trigger_and_synthetic_folders_skip() {
    let h = common::start_with(configure).await;
    let chat = unnamed(&h, "local", "Unsorted").await;
    append(&h, chat, "[kernel] internal", true).await;
    assert!(h.provider.requests().is_empty());
    h.provider.push(Script::reply("Human topic"));
    append(&h, chat, "Real human speech", false).await;
    finished(&h, 1).await;
    assert_eq!(title(&h, chat).await.as_deref(), Some("Human topic"));
    for folder in ["cron", "sleep", "notify", "agents"] {
        let synthetic = unnamed(&h, "system", folder).await;
        append(&h, synthetic, "Not a title task", false).await;
    }
    finished(&h, 5).await;
    assert_eq!(h.provider.requests().len(), 1);
    h.kernel.shared().titles().shutdown().await;
}

#[tokio::test]
async fn cheapest_online_is_default_and_explicit_title_role_wins() {
    let mut mock = common::mock_config();
    let mut cheap = common::model_decl();
    cheap.name = ModelId::from("cheap-title");
    cheap.price = Some(Price {
        input: 0.1,
        output: 0.1,
        cache_read: 0.0,
        cache_write_short: 0.0,
        cache_write_long: 0.0,
    });
    mock.models.push(cheap);
    for configured in [false, true] {
        let h = common::start_with_mock(
            |cfg| {
                configure(cfg);
                cfg.llm.roles.title = configured.then(|| ModelId::from(common::MODEL));
            },
            mock.clone(),
        )
        .await;
        let chat = unnamed(&h, "local", "Unsorted").await;
        h.provider.push(Script::reply("Topic"));
        append(&h, chat, "Describe a topic", false).await;
        finished(&h, 1).await;
        assert_eq!(
            h.provider.requests()[0].model.as_str(),
            if configured {
                common::MODEL
            } else {
                "cheap-title"
            }
        );
        h.kernel.shared().titles().shutdown().await;
    }

    let h = common::start_with(|cfg| {
        configure(cfg);
        cfg.llm.default_model = ModelId::from("offline-default");
    })
    .await;
    let chat = unnamed(&h, "local", "Unsorted").await;
    h.provider.push(Script::reply("Available model"));
    append(&h, chat, "Use an online unpriced model", false).await;
    finished(&h, 1).await;
    assert_eq!(h.provider.requests()[0].model.as_str(), common::MODEL);
    assert_eq!(title(&h, chat).await.as_deref(), Some("Available model"));
    h.kernel.shared().titles().shutdown().await;
}

#[tokio::test]
async fn shutdown_cancels_pending_title_without_waiting_for_provider() {
    let h = common::start_with(configure).await;
    let chat = unnamed(&h, "local", "Unsorted").await;
    let (script, _gate) = Script::new().gate();
    h.provider
        .push(script.text("Late title").stop(StopReason::EndTurn));
    append(&h, chat, "Human speech", false).await;
    turns_support::seen(&h, 1).await;
    tokio::time::timeout(
        Duration::from_secs(1),
        h.kernel.shared().titles().shutdown(),
    )
    .await
    .expect("shutdown");
    assert_eq!(title(&h, chat).await, None);
}
