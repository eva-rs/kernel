mod common;
mod turns_support;

use common::Harness;
use eva::time::Clock as _;
use eva_kernel::{
    db::{Db as _, DbResult, Queries as _, cron::FirePending, stats::Metric},
    subsystems::{
        cron, stats,
        turns::{
            ctx::{self, Entry},
            ports::ToolsetCall,
        },
    },
};
use eva_protocol::{
    types::{ChatId, CronJobId, ToolId, TurnId, tool::ToolContent},
    wire::tools::ToolResult,
};
use serde_json::{Value, json};

async fn invoke(
    h: &Harness,
    chat: ChatId,
    name: &str,
    input: Value,
    master: bool,
    trusted: bool,
) -> ToolResult {
    let entry = Entry::master(chat, turns_support::master(h).await);
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
    let call = ToolsetCall {
        canonical: name,
        call_id: "tools-test",
        input,
        ctx: &ctx.wire(&TurnId::from("tools-test")),
        view: &ctx.view(),
        hot_ids: &[],
        ratio: None,
    };
    if name.starts_with("cron_") {
        cron::tools::call(h.kernel.shared(), call).await.unwrap()
    } else {
        stats::tools::call(h.kernel.shared(), call).await.unwrap()
    }
}

fn text(result: ToolResult) -> String {
    assert!(!result.is_error, "{result:?}");
    let ToolContent::Text { text } = &result.content[0] else {
        panic!("text result")
    };
    text.clone()
}

fn value(result: ToolResult) -> Value {
    serde_json::from_str(&text(result)).unwrap()
}

async fn jobs(h: &Harness) -> Vec<eva_kernel::db::cron::JobRow> {
    h.kernel
        .shared()
        .db()
        .reader()
        .await
        .unwrap()
        .run(|c| c.cron().jobs())
        .await
        .unwrap()
}

#[tokio::test]
async fn cron_validates_before_creating_and_preserves_schedule_on_update() {
    let h = common::start().await;
    let chat = turns_support::chat(&h).await;
    let before: i64 = h
        .kernel
        .shared()
        .db()
        .reader()
        .await
        .unwrap()
        .run(|c| DbResult::Ok(c.query_row("SELECT COUNT(*) FROM chats", [], |r| r.get(0))?))
        .await
        .unwrap();
    for input in [
        json!({"name":"bad", "task":"x"}),
        json!({"name":"bad", "task":"x", "interval":"0s"}),
        json!({"name":"bad", "task":"x", "interval":"1h", "model":"missing"}),
        json!({"name":"bad", "task":"x", "interval":"1h", "tools":["memory_recal"]}),
    ] {
        assert!(
            invoke(&h, chat, "cron_create", input, true, true)
                .await
                .is_error
        );
    }
    let after: i64 = h
        .kernel
        .shared()
        .db()
        .reader()
        .await
        .unwrap()
        .run(|c| DbResult::Ok(c.query_row("SELECT COUNT(*) FROM chats", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(
        before, after,
        "invalid requests must not leave chats behind"
    );
    let deadline =
        eva_kernel::db::stats::later(h.clock.get(), std::time::Duration::from_secs(86_400));
    let created = value(
        invoke(
            &h,
            chat,
            "cron_create",
            json!({"name":"daily", "task":"review",
        "interval":"1h", "expires_at":deadline, "tools":["memory", "memory_recall"]}),
            true,
            true,
        )
        .await,
    );
    let id = created["id"].clone();
    let next = created["next_fire_at"].clone();
    let initial = jobs(&h).await.remove(0);
    assert!(
        initial
            .tools
            .as_ref()
            .unwrap()
            .contains(&"memory_recall".into())
    );
    assert_eq!(
        initial
            .tools
            .as_ref()
            .unwrap()
            .iter()
            .filter(|n| n.as_str() == "memory_recall")
            .count(),
        1
    );
    let updated = value(
        invoke(
            &h,
            chat,
            "cron_update",
            json!({"id":id, "interval":"2h", "at":deadline}),
            true,
            true,
        )
        .await,
    );
    assert_eq!(updated["next_fire_at"], next);
    let row = jobs(&h).await.remove(0);
    assert_eq!(row.expires_at, Some(deadline));
    assert_eq!(row.tools, initial.tools, "omitted keeps whitelist");
    value(
        invoke(
            &h,
            chat,
            "cron_update",
            json!({"id":id, "tools":[]}),
            true,
            true,
        )
        .await,
    );
    assert_eq!(jobs(&h).await[0].tools, Some(vec![]));
    value(
        invoke(
            &h,
            chat,
            "cron_update",
            json!({"id":id, "tools":null, "expires_at":null}),
            true,
            true,
        )
        .await,
    );
    let row = jobs(&h).await.remove(0);
    assert_eq!(row.tools, None);
    assert_eq!(row.expires_at, None);
    assert_eq!(row.next_fire_at, initial.next_fire_at);
    let list = value(invoke(&h, chat, "cron_list", json!({}), true, true).await);
    assert_eq!(list[0]["id"], id);
}

#[tokio::test]
async fn cron_fire_is_persistent_one_shots_shift_and_deletion_archives_chat() {
    let h = common::start().await;
    let chat = turns_support::chat(&h).await;
    let created = value(
        invoke(
            &h,
            chat,
            "cron_create",
            json!({"name":"once", "task":"ping", "at":h.clock.get()}),
            true,
            true,
        )
        .await,
    );
    let id: CronJobId = serde_json::from_value(created["id"].clone()).unwrap();
    let job_chat: ChatId = serde_json::from_value(created["chat_id"].clone()).unwrap();
    value(
        invoke(
            &h,
            chat,
            "cron_fire",
            json!({"id":id, "shift":false}),
            true,
            true,
        )
        .await,
    );
    let now = h.clock.get();
    let due = h
        .kernel
        .shared()
        .db()
        .reader()
        .await
        .unwrap()
        .run(move |c| c.cron().due(now))
        .await
        .unwrap();
    assert_eq!(due[0].fire_pending, Some(FirePending::Shift));
    assert_eq!(due[0].next_fire_at, Some(now));
    value(invoke(&h, chat, "cron_delete", json!({"id":id}), true, true).await);
    assert!(jobs(&h).await.is_empty());
    let row = h
        .kernel
        .shared()
        .db()
        .reader()
        .await
        .unwrap()
        .run(move |c| c.chats().get(job_chat))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.deleted_at, Some(now));
}

#[tokio::test]
async fn tool_gates_reject_direct_calls_and_public_stats_clamp_to_current_chat() {
    let h = common::start().await;
    let chat = turns_support::chat(&h).await;
    let other = turns_support::chat(&h).await;
    assert!(
        invoke(&h, chat, "cron_list", json!({}), true, false)
            .await
            .is_error
    );
    assert!(
        invoke(&h, chat, "cron_list", json!({}), false, true)
            .await
            .is_error
    );
    assert!(
        invoke(&h, chat, "stats_health", json!({}), false, true)
            .await
            .is_error
    );
    let now = h.clock.get();
    let mut own = stats::Recorder::new(h.kernel.shared()).tally(chat, now);
    own.note_call(ToolId(90001), false, false);
    own.finish(false).await.unwrap();
    let mut foreign = stats::Recorder::new(h.kernel.shared()).tally(other, now);
    for _ in 0..5 {
        foreign.note_call(ToolId(90002), false, false);
    }
    foreign.finish(false).await.unwrap();
    let private = text(
        invoke(
            &h,
            chat,
            "stats_tools_usage",
            json!({"scope":"global"}),
            true,
            true,
        )
        .await,
    );
    assert!(private.contains("tools used: 6; distinct: 2"), "{private}");
    let public = text(
        invoke(
            &h,
            chat,
            "stats_tools_usage",
            json!({"scope":"global", "target":"ignored"}),
            true,
            false,
        )
        .await,
    );
    assert!(public.contains("tools used: 1; distinct: 1"), "{public}");
    assert!(!public.contains("90002"), "{public}");
    let redirected = text(
        invoke(
            &h,
            chat,
            "stats_tools_usage",
            json!({"scope":"chat", "target":other.to_string()}),
            true,
            false,
        )
        .await,
    );
    assert_eq!(redirected, public);
    assert!(
        invoke(&h, chat, "stats_tools_usage", json!({"top":0}), true, true)
            .await
            .is_error
    );
}

#[tokio::test]
async fn stats_reports_totals_beyond_top_unpriced_rows_and_alerts() {
    let h = common::start().await;
    let chat = turns_support::chat(&h).await;
    let now = h.clock.get();
    let mut tally = stats::Recorder::new(h.kernel.shared()).tally(chat, now);
    tally.note_call(ToolId(90001), false, false);
    tally.note_call(ToolId(90002), true, true);
    tally.finish(false).await.unwrap();
    let output = text(invoke(&h, chat, "stats_tools_usage", json!({"top":1}), true, true).await);
    assert!(output.contains("tools used: 2; distinct: 2"), "{output}");
    let nanos = now.as_nanos().get() as i64;
    h.kernel.shared().db().write(move |tx| {
        tx.execute("INSERT INTO spend (ts, model, source, op, chat_id, input, cache_read, cache_write, output, cost, currency, cost_source)
            VALUES (?1, 'unpriced-test', 'surface', 'step', ?2, 1, 0, 0, 1, NULL, NULL, 'none'),
                   (?1, 'unpriced-test', 'surface', 'step', ?2, 1, 0, 0, 1, NULL, NULL, 'none')", (nanos, chat.get()))?;
        DbResult::Ok(())
    }).await.unwrap();
    let spend = text(invoke(&h, chat, "stats_spend", json!({}), true, true).await);
    assert!(spend.contains("unpriced: 2 rows"), "{spend}");
    h.kernel.shared().bump(Metric::CronFailed, "daily").await;
    let health = text(invoke(&h, chat, "stats_health", json!({}), true, true).await);
    assert!(health.contains("alerts: cron.failed=1"), "{health}");
    assert!(health.contains("rate_limits:"), "{health}");
}

#[tokio::test]
async fn moved_tools_keep_cron_identity_and_render_current_names() {
    let h = common::start().await;
    let chat = turns_support::chat(&h).await;
    value(
        invoke(
            &h,
            chat,
            "cron_create",
            json!({"name":"moved", "task":"x", "interval":"1h", "tools":["memory_recall"]}),
            true,
            true,
        )
        .await,
    );
    let id = h
        .kernel
        .shared()
        .tools()
        .get()
        .by_canonical("memory_recall")
        .unwrap()
        .id;
    let now = h.clock.get();
    h.kernel
        .shared()
        .db()
        .write(move |tx| {
            tx.tools()
                .put_group("review", "Reviewed tools", "sleep", now)?;
            tx.tools().place(id, "review", "sleep", now)
        })
        .await
        .unwrap();
    let list = jobs(&h).await;
    assert_eq!(list[0].tools, Some(vec!["review_recall".into()]));
    let mut tally = stats::Recorder::new(h.kernel.shared()).tally(chat, now);
    tally.note_call(id, false, false);
    tally.finish(false).await.unwrap();
    let output = text(invoke(&h, chat, "stats_tools_usage", json!({}), true, true).await);
    assert!(output.contains("review_recall"), "{output}");
    assert!(!output.contains("memory_recall"), "{output}");
}

#[tokio::test]
async fn public_health_aggregates_cron_metadata_without_job_names() {
    let h = common::start().await;
    let chat = turns_support::chat(&h).await;
    let secret = "private-meeting-schedule";
    let created = value(
        invoke(
            &h,
            chat,
            "cron_create",
            json!({
                "name":secret, "task":"private task", "interval":"1h",
            }),
            true,
            true,
        )
        .await,
    );
    let id: CronJobId = serde_json::from_value(created["id"].clone()).unwrap();
    let now = h.clock.get();
    let due = eva::time::Timestamp::from_secs(now.as_secs() - 30);
    h.kernel
        .shared()
        .db()
        .write(move |tx| {
            tx.cron().insert_run(&eva_kernel::db::cron::NewRun {
                run: "health-run".into(),
                job: id,
                started: now,
                due: Some(due),
            })
        })
        .await
        .unwrap();
    h.kernel.shared().bump(Metric::CronFailed, secret).await;
    h.kernel
        .shared()
        .bump(Metric::CronFailed, "other-private-schedule")
        .await;
    let private = text(invoke(&h, chat, "stats_health", json!({}), true, true).await);
    assert!(private.contains(secret), "{private}");
    let public = text(invoke(&h, chat, "stats_health", json!({}), true, false).await);
    assert!(
        !public.contains(secret) && !public.contains("other-private-schedule"),
        "{public}"
    );
    assert!(public.contains("cron_fire_lag max: 30s"), "{public}");
    assert!(public.contains("cron.failed |  | 2"), "{public}");
    assert!(public.contains("alerts: cron.failed=2"), "{public}");
}
