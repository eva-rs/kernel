mod common;
mod turns_support;
use eva_kernel::subsystems::turns::log::{EndedBy, Payload};
use eva_protocol::types::{
    chat::Role,
    generate::{ProviderError, ProviderErrorKind, StopReason, Usage},
    turn::{FeedEvent, TurnOutcome},
};
use eva_providers::mock::Script;
use turns_support::*;

#[tokio::test]
async fn end_turn_writes_turn_ended_and_turn_done() {
    let h = strict().await;
    let chat = chat(&h).await;
    let mut rx = h.kernel.shared().turns().slot(chat).feed.subscribe();
    h.provider.push(Script::reply("done").usage(Usage {
        input: 1200,
        output: 8,
        ..Default::default()
    }));
    append(&h, chat, "hello").await;
    assert_eq!(done(&mut rx).await, (TurnOutcome::Completed, None));
    let events = events(&h, chat).await;
    assert!(
        matches!(&events.last().unwrap().payload,Payload::TurnEnded(t) if t.outcome==TurnOutcome::Completed)
    );
    assert_eq!(h.provider.requests().len(), 1);
    assert!(h.kernel.shared().turns().live(chat).is_none());
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.payload, Payload::ParamsChanged(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn drain_step_sees_reply_sent_during_last_step() {
    let h = strict().await;
    let chat = chat(&h).await;
    let mut rx = h.kernel.shared().turns().slot(chat).feed.subscribe();
    let (script, gate) = Script::new().text("first").gate();
    h.provider.push(script.stop(StopReason::EndTurn));
    h.provider.push(Script::reply("second"));
    append(&h, chat, "initial").await;
    seen(&h, 1).await;
    append(&h, chat, "arrived while thinking").await;
    gate.open();
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Completed);
    let req = h.provider.requests();
    assert_eq!(req.len(), 2);
    assert!(format!("{:?}", req[1].messages).contains("arrived while thinking"));
    let events = events(&h, chat).await;
    let user_turns: Vec<_> = events
        .iter()
        .filter(
            |e| matches!(&e.payload,Payload::Message(m) if m.role==Role::User&&m.speaker.is_some()),
        )
        .map(|e| e.turn_id.clone())
        .collect();
    assert_eq!(user_turns[0], user_turns[1]);
}

#[tokio::test]
async fn max_tokens_continue_once_then_completed() {
    let h = strict().await;
    let chat = chat(&h).await;
    let mut rx = h.kernel.shared().turns().slot(chat).feed.subscribe();
    for _ in 0..2 {
        h.provider
            .push(Script::new().text("truncated").stop(StopReason::MaxTokens));
    }
    append(&h, chat, "continue").await;
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Completed);
    assert_eq!(h.provider.requests().len(), 2);
    let events = events(&h, chat).await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.payload, Payload::SystemText(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn transient_after_delta_persists_partial_and_continues() {
    let h = strict().await;
    let chat = chat(&h).await;
    let mut rx = h.kernel.shared().turns().slot(chat).feed.subscribe();
    h.provider
        .push(Script::new().text("partial").fail(ProviderError {
            kind: ProviderErrorKind::Transient,
            http_status: Some(502),
            raw: "disconnected".into(),
            retry_after: None,
        }));
    h.provider.push(Script::reply("continued"));
    append(&h, chat, "hello").await;
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Completed);
    let events = events(&h, chat).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.payload,Payload::Message(m) if m.partial))
    );
    assert_eq!(h.provider.requests().len(), 2);
}

#[tokio::test]
async fn unknown_tool_lists_nearest_and_find_hint() {
    let h = strict().await;
    let chat = chat(&h).await;
    let mut rx = h.kernel.shared().turns().slot(chat).feed.subscribe();
    h.provider.push(Script::call(
        "call1",
        "tools_fnd",
        serde_json::json!({"query":"read"}),
    ));
    h.provider.push(Script::reply("fixed"));
    append(&h, chat, "search tools").await;
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Completed);
    let events = events(&h, chat).await;
    assert!(events.iter().any(|e|matches!(&e.payload,Payload::ToolResult(r) if r.is_error && format!("{:?}",r.content).contains("tools_find"))));
}

#[tokio::test]
async fn step_limit_completes_with_by_step_limit_and_wrapup_reminder() {
    let h = common::start_with(|c| {
        c.work.max_steps_hard = 2;
        c.work.wrapup_steps = 2;
    })
    .await;
    let chat = chat(&h).await;
    let mut rx = h.kernel.shared().turns().slot(chat).feed.subscribe();
    for i in 0..2 {
        h.provider.push(Script::call(
            format!("c{i}"),
            "unknown",
            serde_json::json!({}),
        ));
    }
    append(&h, chat, "work").await;
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Completed);
    let events = events(&h, chat).await;
    assert!(
        matches!(&events.last().unwrap().payload,Payload::TurnEnded(t) if t.by==Some(EndedBy::StepLimit))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.payload,Payload::Reminder(r) if r.text.contains("[wrapup]")))
    );
}

#[tokio::test]
async fn item_completed_replaces_live_id_same_turn() {
    let h = strict().await;
    let chat = chat(&h).await;
    let mut rx = h.kernel.shared().turns().slot(chat).feed.subscribe();
    h.provider.push(Script::reply("answer"));
    append(&h, chat, "hello").await;
    let mut live = None;
    let mut replacement = None;
    loop {
        match rx.recv().await.unwrap().item.event {
            FeedEvent::ItemStarted { item }
                if matches!(
                    item.body,
                    eva_protocol::types::turn::ItemBody::AgentMessage { .. }
                ) =>
            {
                live = Some(item.id)
            }
            FeedEvent::ItemCompleted {
                replaces: Some(id), ..
            } => replacement = Some(id),
            FeedEvent::TurnDone { .. } => break,
            _ => {}
        }
    }
    assert!(live.is_some());
    assert_eq!(live, replacement);
}

#[tokio::test]
async fn recovery_closes_open_turns_interrupted_and_cancels_pending() {
    use eva_kernel::{
        db::{Db as _, DbResult, Queries as _, agents::PendingRow},
        subsystems::turns::{
            compaction, ctx,
            log::{self, Append, ToolUsePayload},
            recovery,
        },
    };
    use eva_protocol::types::{
        PendingId, TurnId,
        turn::{PendingKind, PendingOutcome},
    };
    let h = strict().await;
    let chat = chat(&h).await;
    let entry = ctx::Entry::master(chat, master(&h).await);
    let llm = h.kernel.shared().cfg().llm.clone();
    let ctx = h
        .kernel
        .shared()
        .db()
        .reader()
        .await
        .unwrap()
        .run(move |c| ctx::resolve(c, entry, &llm))
        .await
        .unwrap();
    let segment = compaction::ensure_segment(h.kernel.shared(), &ctx)
        .await
        .unwrap();
    let now = h.kernel.shared().now();
    h.kernel
        .shared()
        .db()
        .write(move |tx| {
            let turn = TurnId::from("crashed");
            let at = Append {
                chat,
                segment,
                turn_id: Some(&turn),
                step: Some(1),
                now,
            };
            log::append(
                tx,
                at,
                &Payload::ToolUse(ToolUsePayload {
                    call_id: "call".into(),
                    tool_id: None,
                    name: "missing".into(),
                    input: serde_json::json!({}),
                }),
            )?;
            tx.agents().insert_pending(&PendingRow {
                id: PendingId::from("pending-crash"),
                chat,
                turn,
                call_id: "call".into(),
                kind: PendingKind::Ask,
                payload: serde_json::json!({}),
                created_at: now,
                expires_at: now + std::time::Duration::from_secs(60),
                resolved_at: None,
                outcome: None,
            })?;
            DbResult::Ok(())
        })
        .await
        .unwrap();
    recovery::recover(h.kernel.shared()).await.unwrap();
    recovery::recover(h.kernel.shared()).await.unwrap();
    let events = events(&h, chat).await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.payload, Payload::TurnEnded(_)))
            .count(),
        1
    );
    assert!(events.iter().any(|e|matches!(&e.payload,Payload::ToolResult(r) if r.synthetic==Some(eva_kernel::subsystems::turns::log::Synthetic::Interrupted))));
    let p = h
        .kernel
        .shared()
        .db()
        .reader()
        .await
        .unwrap()
        .run(|c| c.agents().pending(&PendingId::from("pending-crash")))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.outcome, Some(PendingOutcome::Cancelled));
}
