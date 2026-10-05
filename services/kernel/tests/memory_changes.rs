mod common;
mod turns_support;

use common::Harness;
use eva_core::{CallStep as _, service::memory::Memory as _};
use eva_kernel::{
    db::{Db as _, DbResult, Queries as _},
    subsystems::{
        memory::{
            self,
            tools::{self, TurnMemory},
        },
        turns::{
            ctx::Entry,
            ingest::{self, Message},
            log::Payload,
        },
    },
};
use eva_protocol::{
    requests::memory::restore,
    types::{
        ChatId, PersonId,
        content::Block,
        memory::{
            Kind, Visibility,
            tools::{forget, remember},
        },
        turn::{Audience, TurnView},
    },
};

fn args(text: &str) -> remember::Args {
    remember::Args {
        kind: Kind::Core,
        text: text.into(),
        visibility: Some(Visibility::Private),
        importance: None,
        occurred_at: None,
        valid_from: None,
        tags: vec![],
        name: None,
        description: None,
    }
}
fn view(chat: ChatId) -> TurnView {
    TurnView {
        chat,
        actor: Some(PersonId::system()),
        master_turn: true,
        trusted_chat: true,
        audience: Audience::MasterOnly,
    }
}
fn context<'a>(h: &'a Harness, view: &'a TurnView) -> TurnMemory<'a, common::Test> {
    TurnMemory {
        shared: h.kernel.shared(),
        view,
        surface: None,
        run_id: None,
        hot_ids: &[],
        ratio: None,
    }
}
async fn open(h: &Harness, chat: ChatId) {
    ingest::append(
        h.kernel.shared(),
        Entry::master(chat, PersonId::system()),
        Message {
            content: vec![Block::text("seed")],
            external_id: None,
            reply_to_external_id: None,
            silent: true,
            machine: true,
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn core_changes_reach_only_visible_contexts_without_rewriting_snapshots() {
    let h = common::start().await;
    let trusted = turns_support::chat(&h).await;
    let public = turns_support::chat(&h).await;
    h.kernel
        .shared()
        .db()
        .write(move |tx| -> DbResult<()> {
            tx.execute("UPDATE chats SET trusted=0 WHERE id=?1", [public.0])?;
            Ok(())
        })
        .await
        .unwrap();
    open(&h, trusted).await;
    open(&h, public).await;
    let before = h
        .query(move |c| c.turns().open_segment(trusted))
        .await
        .unwrap()
        .system_snapshot;
    let v = view(trusted);
    let ctx = context(&h, &v);
    let saved = tools::remember(&ctx, args("Never disclose the private cabinet code"))
        .await
        .unwrap();
    let changes = turns_support::events(&h, trusted).await;
    assert!(changes.iter().any(|e|matches!(&e.payload,Payload::MemoryUpdated(m) if m.added.len()==1 && m.added[0].contains("private cabinet"))));
    assert!(
        !turns_support::events(&h, public)
            .await
            .iter()
            .any(|e| matches!(e.payload, Payload::MemoryUpdated(_)))
    );
    tools::forget(
        &ctx,
        forget::Args {
            id: saved.id,
            why: "obsolete".into(),
        },
    )
    .await
    .unwrap();
    assert!(
        turns_support::events(&h, trusted)
            .await
            .iter()
            .any(|e| matches!(&e.payload,Payload::MemoryUpdated(m) if m.removed==vec![saved.id]))
    );
    h.client_session()
        .await
        .memory()
        .restore()
        .call(restore::Args { id: saved.id })
        .await
        .unwrap();
    assert_eq!(
        turns_support::events(&h, trusted)
            .await
            .iter()
            .filter(|e| matches!(e.payload, Payload::MemoryUpdated(_)))
            .count(),
        3
    );
    assert_eq!(
        h.query(move |c| c.turns().open_segment(trusted))
            .await
            .unwrap()
            .system_snapshot,
        before
    );
}

#[tokio::test]
async fn core_overflow_notification_is_throttled_across_chats() {
    let h = common::start_with(|cfg| cfg.memory.budget_tokens = 80).await;
    let chat = turns_support::chat(&h).await;
    let v = view(chat);
    let ctx = context(&h, &v);
    tools::remember(
        &ctx,
        args(
            "A core instruction that consumes more than the small memory budget allows. "
                .repeat(4)
                .as_str(),
        ),
    )
    .await
    .unwrap();
    let one = memory::hot_block(h.kernel.shared(), &v, None)
        .await
        .unwrap();
    assert!(one.dropped.core > 0);
    memory::hot_block(h.kernel.shared(), &v, None)
        .await
        .unwrap();
    assert_eq!(h.counter("notify.sent").await, 1);
    h.clock
        .clone()
        .advance(std::time::Duration::from_secs(86_401));
    memory::hot_block(h.kernel.shared(), &v, None)
        .await
        .unwrap();
    assert_eq!(h.counter("notify.sent").await, 2);
}
