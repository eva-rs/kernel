mod common;
mod turns_support;
use eva_protocol::types::{
    module::ModuleKind,
    turn::{FeedEvent, ItemBody, PendingKind, PendingOutcome, Pick, TurnOutcome},
};
use eva_providers::mock::Script;
use eva_soul::{
    db::{Db as _, Queries as _},
    subsystems::turns::{
        log::{EndedBy, Payload},
        pending::{self, Answered},
    },
};
use turns_support::*;

async fn ask(
    h: &common::Harness,
    chat: eva_protocol::types::ChatId,
    rx: &mut tokio::sync::broadcast::Receiver<eva_soul::subsystems::turns::feed::FeedMsg>,
) -> eva_soul::db::agents::PendingRow {
    h.live_session(common::CLIENT, ModuleKind::Client, true);
    h.provider.push(Script::call("ask1","chat_ask",serde_json::json!({"questions":[{"text":"Which?","options":[{"label":"A"},{"label":"B"}]}]})));
    h.provider.push(Script::reply("received"));
    append(h, chat, "ask me").await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let FeedEvent::ItemStarted { item } = rx.recv().await.unwrap().item.event
                && let ItemBody::ClientOp {
                    pending,
                    kind: PendingKind::Ask,
                    ..
                } = item.body
            {
                return h
                    .soul
                    .shared()
                    .db()
                    .reader()
                    .await
                    .unwrap()
                    .run(move |c| c.agents().pending(&pending))
                    .await
                    .unwrap()
                    .unwrap();
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn ask_answered_and_respond_twice_conflicts() {
    let h = strict().await;
    let chat = chat(&h).await;
    let mut rx = h.soul.shared().turns().slot(chat).feed.subscribe();
    let row = ask(&h, chat, &mut rx).await;
    assert!(
        pending::resolve(
            h.soul.shared(),
            &row,
            PendingOutcome::Answered,
            Some(Answered::Picks(vec![Pick {
                question: 0,
                option: 1
            }]))
        )
        .await
        .unwrap()
    );
    assert!(
        !pending::resolve(h.soul.shared(), &row, PendingOutcome::Answered, None)
            .await
            .unwrap()
    );
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Completed);
    assert!(events(&h,chat).await.iter().any(|e|matches!(&e.payload,Payload::ToolResult(r) if !r.is_error&&format!("{:?}",r.content).contains("B"))));
}

#[tokio::test]
async fn ask_text_instead_consumed_by() {
    let h = strict().await;
    let chat = chat(&h).await;
    let mut rx = h.soul.shared().turns().slot(chat).feed.subscribe();
    let row = ask(&h, chat, &mut rx).await;
    append(&h, chat, "my own answer").await;
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Completed);
    let pending = h
        .soul
        .shared()
        .db()
        .reader()
        .await
        .unwrap()
        .run(move |c| c.agents().pending(&row.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.outcome, Some(PendingOutcome::TextInstead));
    assert!(events(&h, chat).await.iter().any(
        |e| matches!(&e.payload,Payload::Message(m) if m.consumed_by.as_deref()==Some("ask1"))
    ));
}

#[tokio::test]
async fn cancel_during_pending_closes_request() {
    let h = strict().await;
    let chat = chat(&h).await;
    let mut rx = h.soul.shared().turns().slot(chat).feed.subscribe();
    let row = ask(&h, chat, &mut rx).await;
    h.soul.shared().turns().cancel(chat, EndedBy::Shutdown);
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Cancelled);
    let pending = h
        .soul
        .shared()
        .db()
        .reader()
        .await
        .unwrap()
        .run(move |c| c.agents().pending(&row.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.outcome, Some(PendingOutcome::Cancelled));
}
