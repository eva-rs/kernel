#![allow(dead_code)]
use crate::common::{self, Harness};
use eva_kernel::{
    db::{Db as _, DbResult, Queries as _},
    subsystems::turns::{
        ctx::Entry,
        feed::FeedMsg,
        ingest::{self, Message},
        log::{self, Event},
    },
};
use eva_protocol::types::{
    ChatId, PersonId,
    content::Block,
    turn::{FeedEvent, TurnOutcome},
};
use std::time::Duration;

pub async fn chat(h: &Harness) -> ChatId {
    h.kernel.shared().db().write(|tx| DbResult::Ok(ChatId(tx.query_row("INSERT INTO chats (folder_id, kind, title, trusted, created_at, last_activity_at) SELECT id, 'persistent', 'Test chat', 1, 1, 1 FROM folders WHERE surface_id = 'local' AND name = 'Unsorted' RETURNING id", [], |r| r.get(0))?))).await.expect("chat")
}
pub async fn master(h: &Harness) -> PersonId {
    h.kernel
        .shared()
        .db()
        .reader()
        .await
        .expect("reader")
        .run(|c| {
            DbResult::Ok(PersonId(c.query_row(
                "SELECT id FROM persons WHERE master = 1 LIMIT 1",
                [],
                |r| r.get(0),
            )?))
        })
        .await
        .expect("master")
}
pub async fn append(h: &Harness, chat: ChatId, text: &str) {
    ingest::append(
        h.kernel.shared(),
        Entry::master(chat, master(h).await),
        Message {
            content: vec![Block::text(text)],
            external_id: None,
            reply_to_external_id: None,
            silent: false,
            machine: false,
        },
    )
    .await
    .expect("append");
}
pub async fn done(
    rx: &mut tokio::sync::broadcast::Receiver<FeedMsg>,
) -> (TurnOutcome, Option<String>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let FeedEvent::TurnDone { outcome, error, .. } =
                rx.recv().await.expect("feed").item.event
            {
                return (outcome, error);
            }
        }
    })
    .await
    .expect("turn finished")
}
pub async fn events(h: &Harness, chat: ChatId) -> Vec<Event> {
    let rows = h
        .kernel
        .shared()
        .db()
        .reader()
        .await
        .expect("reader")
        .run(move |c| c.turns().events_after(chat, None))
        .await
        .expect("events");
    log::decode_all(rows).expect("decode")
}
pub async fn seen(h: &Harness, n: usize) {
    let mut rx = h.provider.seen();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if *rx.borrow_and_update() >= n {
                return;
            }
            rx.changed().await.expect("provider");
        }
    })
    .await
    .expect("request");
}
pub async fn strict() -> Harness {
    let mut cfg = common::mock_config();
    cfg.on_exhausted = eva_providers::mock::Exhausted::Fail;
    common::start_with_mock(|cfg| cfg.context.count_prefix = false, cfg).await
}
