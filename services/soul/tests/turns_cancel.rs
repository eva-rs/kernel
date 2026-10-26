mod common;
mod turns_support;
use eva_protocol::types::{generate::StopReason, turn::TurnOutcome};
use eva_providers::mock::Script;
use eva_soul::subsystems::turns::log::{EndedBy, Payload, Synthetic};
use turns_support::*;

#[tokio::test]
async fn cancel_mid_stream_partial_and_not_started() {
    let h = strict().await;
    let chat = chat(&h).await;
    let mut rx = h.soul.shared().turns().slot(chat).feed.subscribe();
    let (script, gate) = Script::new()
        .text("partial")
        .tool_use("c1", "tools_find", serde_json::json!({"query":"file"}))
        .gate();
    h.provider.push(script.stop(StopReason::ToolUse));
    append(&h, chat, "work").await;
    // Дельта в ленте доказывает, что generate успел отдать часть ответа.
    loop {
        if matches!(
            rx.recv().await.unwrap().item.event,
            eva_protocol::types::turn::FeedEvent::ItemDelta { .. }
        ) {
            break;
        }
    }
    tokio::task::yield_now().await;
    h.soul.shared().turns().cancel(chat, EndedBy::Shutdown);
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Cancelled);
    drop(gate);
    let events = events(&h, chat).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.payload,Payload::Message(m) if m.partial))
    );
    assert!(
        matches!(&events.last().unwrap().payload,Payload::TurnEnded(t) if t.by==Some(EndedBy::Shutdown))
    );
    for event in &events {
        if let Payload::ToolUse(call) = &event.payload {
            assert!(events.iter().any(|e|matches!(&e.payload,Payload::ToolResult(r) if r.call_id==call.call_id&&r.synthetic==Some(Synthetic::NotStarted))));
        }
    }
}

#[tokio::test]
async fn shutdown_cancels_all_live_turns_before_db_close() {
    let h = strict().await;
    let chat = chat(&h).await;
    let mut rx = h.soul.shared().turns().slot(chat).feed.subscribe();
    let (script, gate) = Script::new().gate();
    h.provider.push(script.stop(StopReason::EndTurn));
    append(&h, chat, "wait").await;
    seen(&h, 1).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        h.soul.shared().turns().shutdown(),
    )
    .await
    .unwrap();
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Cancelled);
    drop(gate);
}

#[tokio::test]
async fn cancel_during_compaction_preserves_old_segment() {
    use eva_soul::db::{Db as _, Queries as _};
    let h = common::start_with(|c| {
        c.tools.core.push("context".into());
        c.context.count_prefix = false;
    })
    .await;
    let chat = chat(&h).await;
    let mut rx = h.soul.shared().turns().slot(chat).feed.subscribe();
    h.provider.push(Script::call(
        "compact",
        "context_compact",
        serde_json::json!({}),
    ));
    let (summary, gate) = Script::new().gate();
    h.provider
        .push(summary.text("summary").stop(StopReason::EndTurn));
    append(&h, chat, "remember this before compacting").await;
    seen(&h, 2).await;
    let before = h
        .soul
        .shared()
        .db()
        .reader()
        .await
        .unwrap()
        .run(move |c| c.turns().open_segment(chat))
        .await
        .unwrap()
        .unwrap()
        .id;
    h.soul.shared().turns().cancel(chat, EndedBy::Shutdown);
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Cancelled);
    drop(gate);
    let after = h
        .soul
        .shared()
        .db()
        .reader()
        .await
        .unwrap()
        .run(move |c| c.turns().open_segment(chat))
        .await
        .unwrap()
        .unwrap()
        .id;
    assert_eq!(before, after);
}
