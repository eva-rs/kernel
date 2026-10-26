//! Ручная правка блокнота и полные снимки общие с модельными мутациями.

mod common;
mod turns_support;

use common::Harness;
use eva::{handling::CallStep as _, time::Clock as _};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::Notes::{Trait as _, history, list, put, restore, snapshot},
    types::{ChatId, note::Mode, turn::TurnOutcome},
};
use eva_providers::mock::Script;
use eva_soul::{
    db::{Db as _, DbResult, Queries as _},
    subsystems::turns::log::Payload,
};
use serde_json::json;
use turns_support::{append, chat, done, events, seen, strict};

fn args(chat: ChatId, name: &str, body: &str) -> put::Args {
    put::Args {
        chat,
        name: name.into(),
        description: format!("about {name}"),
        body: body.into(),
        pinned: None,
    }
}

async fn put(h: &Harness, chat: ChatId, name: &str, body: &str) -> put::Ok {
    h.client_session()
        .await
        .notes()
        .inner_mut()
        .put()
        .call(args(chat, name, body))
        .await
        .expect("put")
}

async fn list(h: &Harness, chat: ChatId) -> list::Ok {
    h.client_session()
        .await
        .notes()
        .list(chat)
        .await
        .expect("list")
}

async fn history(h: &Harness, chat: ChatId) -> history::Ok {
    h.client_session()
        .await
        .notes()
        .history(chat, None, Default::default())
        .await
        .expect("history")
}

async fn snapshot(h: &Harness, chat: ChatId, id: i64) -> snapshot::Ok {
    h.client_session()
        .await
        .notes()
        .snapshot(chat, id)
        .await
        .expect("snapshot")
}

async fn restore(h: &Harness, chat: ChatId, id: i64) -> restore::Ok {
    h.client_session()
        .await
        .notes()
        .restore(chat, id)
        .await
        .expect("restore")
}

#[tokio::test]
async fn full_notebook_restore_recovers_deleted_notes_and_can_undo_itself() {
    let h = common::start().await;
    let chat = chat(&h).await;
    put(&h, chat, "a", "original a").await;
    let mut b = args(chat, "b", "original b");
    b.pinned = Some(true);
    h.client_session()
        .await
        .notes()
        .inner_mut()
        .put()
        .call(b)
        .await
        .map(|ok| ok.0)
        .unwrap();
    let original = list(&h, chat).await.notes;
    put(&h, chat, "a", "changed a").await;
    let old = history(&h, chat).await.snapshots[0].id;
    h.client_session()
        .await
        .notes()
        .delete(chat, "b".into())
        .await
        .map(|ok| ok.0)
        .unwrap();
    put(&h, chat, "c", "new c").await;
    let changed = list(&h, chat).await.notes;

    let old_snapshot = snapshot(&h, chat, old).await;
    assert_eq!(old_snapshot.snapshot.count, 2);
    assert_eq!(old_snapshot.notes, original);
    let restored = restore(&h, chat, old).await;
    assert_eq!(list(&h, chat).await.notes, original);
    assert_eq!(snapshot(&h, chat, restored.backup).await.notes, changed);
    assert_eq!(restored.fill.count, 2);

    let reverted = restore(&h, chat, restored.backup).await;
    assert_eq!(list(&h, chat).await.notes, changed);
    assert_eq!(snapshot(&h, chat, reverted.backup).await.notes, original);
    let empty = history(&h, chat).await.snapshots.last().unwrap().id;
    let empty_snapshot = snapshot(&h, chat, empty).await;
    assert_eq!(empty_snapshot.snapshot.id, empty);
    assert_eq!(empty_snapshot.snapshot.count, 0);
    assert!(empty_snapshot.notes.is_empty());
    restore(&h, chat, empty).await;
    assert!(list(&h, chat).await.notes.is_empty());
}

#[tokio::test]
async fn model_save_update_and_delete_checkpoint_the_entire_notebook() {
    let h = strict().await;
    let chat = chat(&h).await;
    put(&h, chat, "other", "untouched").await;
    let mut rx = h.soul.shared().turns().slot(chat).feed.subscribe();
    h.provider
        .push(Script::call(
            "save",
            "notes_save",
            json!({"name":"job", "description":"work", "body":"first"}),
        ))
        .push(Script::call(
            "update",
            "notes_update",
            json!({"name":"job", "body":"second", "pinned":true}),
        ))
        .push(Script::call(
            "delete",
            "notes_delete",
            json!({"name":"job"}),
        ))
        .push(Script::reply("done"));
    append(&h, chat, "change the notebook").await;
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Completed);
    let history = history(&h, chat).await.snapshots;
    assert_eq!(
        history
            .iter()
            .map(|s| s.reason.as_str())
            .collect::<Vec<_>>(),
        ["delete", "update", "save", "save"]
    );
    let before_delete = snapshot(&h, chat, history[0].id).await.notes;
    assert_eq!(before_delete.len(), 2);
    assert!(
        before_delete
            .iter()
            .any(|n| n.name == "job" && n.body == "second" && n.pinned)
    );
    assert!(
        before_delete
            .iter()
            .any(|n| n.name == "other" && n.body == "untouched")
    );
    let before_update = snapshot(&h, chat, history[1].id).await.notes;
    assert!(
        before_update
            .iter()
            .any(|n| n.name == "job" && n.body == "first" && !n.pinned)
    );
    assert_eq!(list(&h, chat).await.notes.len(), 1);
    restore(&h, chat, history[0].id).await;
    assert_eq!(list(&h, chat).await.notes, before_delete);
}

#[tokio::test]
async fn retention_and_history_cursor_are_per_chat() {
    let h = common::start_with(|cfg| cfg.notes.history_keep = 2).await;
    let one = chat(&h).await;
    let two = chat(&h).await;
    put(&h, two, "stable", "other chat").await;
    put(&h, one, "a", "first").await;
    let expired = history(&h, one).await.snapshots[0].id;
    put(&h, one, "a", "second").await;
    put(&h, one, "a", "third").await;
    let snapshots = history(&h, one).await.snapshots;
    assert_eq!(snapshots.len(), 2);
    let page = h
        .client_session()
        .await
        .notes()
        .history(one, Some(snapshots[0].id), Default::default())
        .await
        .unwrap();
    assert_eq!(page.snapshots, snapshots[1..]);
    let result = h
        .client_session()
        .await
        .notes()
        .snapshot(one, expired)
        .await;
    assert!(matches!(result, Err(Generic::Aux(Aux::NotFound(_)))));
    assert_eq!(history(&h, two).await.snapshots.len(), 1);
    let orphans: i64 = h.query(|c| Ok(c.query_row(
        "SELECT count(*) FROM notes_snapshot_entries e LEFT JOIN notes_snapshots s ON s.id=e.snapshot_id WHERE s.id IS NULL", [], |r| r.get(0),
    )?)).await;
    assert_eq!(orphans, 0);
}

#[tokio::test]
async fn rejected_edits_and_failed_writes_leave_history_unchanged() {
    let h = common::start().await;
    let chat = chat(&h).await;
    put(&h, chat, "a", "original").await;
    let old_notes = list(&h, chat).await.notes;
    let old_history = history(&h, chat).await.snapshots;
    let invalid = h
        .client_session()
        .await
        .notes()
        .inner_mut()
        .put()
        .call(args(chat, "a", " "))
        .await
        .map(|ok| ok.0);
    assert!(matches!(invalid, Err(Generic::Aux(Aux::Invalid(_)))));
    let missing = h
        .client_session()
        .await
        .notes()
        .delete(chat, "missing".into())
        .await
        .map(|ok| ok.0);
    assert!(matches!(missing, Err(Generic::Aux(Aux::NotFound(_)))));
    h.soul.shared().db().write(|tx| -> DbResult<()> {
        tx.execute_batch("CREATE TRIGGER fail_note_update BEFORE UPDATE ON chat_notes BEGIN SELECT RAISE(ABORT, 'test failure'); END;")?;
        Ok(())
    }).await.unwrap();
    let failed = h
        .client_session()
        .await
        .notes()
        .inner_mut()
        .put()
        .call(args(chat, "a", "new"))
        .await
        .map(|ok| ok.0);
    assert!(matches!(failed, Err(Generic::Aux(Aux::Db(_)))));
    assert_eq!(list(&h, chat).await.notes, old_notes);
    assert_eq!(history(&h, chat).await.snapshots, old_history);
}

#[tokio::test]
async fn owner_only_and_snapshot_ids_cannot_cross_chats() {
    let h = common::start().await;
    let one = chat(&h).await;
    let two = chat(&h).await;
    put(&h, one, "a", "private").await;
    let id = history(&h, one).await.snapshots[0].id;
    let mut service = h.service_with_surface(common::SVC).await;
    assert!(matches!(
        service
            .notes()
            .inner_mut()
            .put()
            .call(args(one, "a", "bad"))
            .await
            .map(|ok| ok.0),
        Err(Generic::Aux(Aux::Forbidden(_)))
    ));
    assert!(matches!(
        service.notes().delete(one, "a".into()).await.map(|ok| ok.0),
        Err(Generic::Aux(Aux::Forbidden(_)))
    ));
    assert!(matches!(
        service.notes().history(one, None, Default::default()).await,
        Err(Generic::Aux(Aux::Forbidden(_)))
    ));
    assert!(matches!(
        service.notes().snapshot(one, id).await,
        Err(Generic::Aux(Aux::Forbidden(_)))
    ));
    assert!(matches!(
        service.notes().restore(one, id).await,
        Err(Generic::Aux(Aux::Forbidden(_)))
    ));
    let mut owner = h.client_session().await;
    assert!(matches!(
        owner.notes().snapshot(two, id).await,
        Err(Generic::Aux(Aux::NotFound(_)))
    ));
    assert!(matches!(
        owner.notes().restore(two, id).await,
        Err(Generic::Aux(Aux::NotFound(_)))
    ));
    for id in [-1, 0] {
        assert!(matches!(
            owner.notes().snapshot(one, id).await,
            Err(Generic::Aux(Aux::Invalid(_)))
        ));
        assert!(matches!(
            owner.notes().restore(one, id).await,
            Err(Generic::Aux(Aux::Invalid(_)))
        ));
    }
    assert_eq!(history(&h, one).await.snapshots.len(), 1);
    assert!(history(&h, two).await.snapshots.is_empty());
}

#[tokio::test]
async fn external_edit_conflicts_with_live_turn_and_refreshes_next_context() {
    let h = strict().await;
    let chat = chat(&h).await;
    put(&h, chat, "a", "before edit").await;
    let id = history(&h, chat).await.snapshots[0].id;
    let mut rx = h.soul.shared().turns().slot(chat).feed.subscribe();
    let (script, gate) = Script::reply("done").gate();
    h.provider.push(script);
    append(&h, chat, "start").await;
    seen(&h, 1).await;
    let mut owner = h.client_session().await;
    assert!(matches!(
        owner
            .notes()
            .inner_mut()
            .put()
            .call(args(chat, "a", "during turn"))
            .await
            .map(|ok| ok.0),
        Err(Generic::Aux(Aux::Conflict(_)))
    ));
    assert!(matches!(
        owner.notes().delete(chat, "a".into()).await.map(|ok| ok.0),
        Err(Generic::Aux(Aux::Conflict(_)))
    ));
    assert!(matches!(
        owner.notes().restore(chat, id).await,
        Err(Generic::Aux(Aux::Conflict(_)))
    ));
    assert_eq!(history(&h, chat).await.snapshots.len(), 1);
    gate.open();
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Completed);
    put(&h, chat, "a", "after owner edit").await;
    assert!(events(&h, chat).await.iter().any(|e| matches!(&e.payload, Payload::NoteUpdated(note) if note.entry.contains("after owner edit"))));
    h.provider.push(Script::reply("seen"));
    append(&h, chat, "continue").await;
    assert_eq!(done(&mut rx).await.0, TurnOutcome::Completed);
    assert!(format!("{:?}", h.provider.requests().last().unwrap()).contains("after owner edit"));
}

#[tokio::test]
async fn concurrent_edits_are_serialized_with_complete_checkpoints() {
    let h = common::start().await;
    let chat = chat(&h).await;
    let (a, b) = tokio::join!(put(&h, chat, "a", "body a"), put(&h, chat, "b", "body b"));
    assert!(a.0.fill.count == 1 || b.0.fill.count == 1);
    assert!(a.0.fill.count == 2 || b.0.fill.count == 2);
    assert_eq!(list(&h, chat).await.notes.len(), 2);
    let history = history(&h, chat).await.snapshots;
    assert_eq!(history.iter().map(|s| s.count).collect::<Vec<_>>(), [1, 0]);
}

#[tokio::test]
async fn owner_edits_keep_index_mode_and_apply_existing_limits() {
    let h = common::start_with(|cfg| cfg.notes.max_notes = 1).await;
    let chat = chat(&h).await;
    put(&h, chat, "a", "old").await;
    h.soul.shared().db().write(move |tx| -> DbResult<()> {
        tx.execute("INSERT INTO notes_state (chat_id, mode, since_at, squeeze_turns) VALUES (?1, 'index', 1, 0)", [chat.get()])?;
        Ok(())
    }).await.unwrap();
    put(&h, chat, "a", "owner does not need a model notes_read").await;
    assert_eq!(list(&h, chat).await.mode, Mode::Index);
    let before = history(&h, chat).await.snapshots;
    let rejected = h
        .client_session()
        .await
        .notes()
        .inner_mut()
        .put()
        .call(args(chat, "b", "too many"))
        .await
        .map(|ok| ok.0);
    assert!(matches!(rejected, Err(Generic::Aux(Aux::Invalid(_)))));
    assert_eq!(history(&h, chat).await.snapshots, before);
    assert_eq!(list(&h, chat).await.notes.len(), 1);
    assert!(
        eva_soul::config::NotesConfig {
            history_keep: 0,
            ..Default::default()
        }
        .check()
        .is_err()
    );
}

#[tokio::test]
async fn restore_checks_current_limits_before_creating_a_backup() {
    let h = common::start_with(|cfg| cfg.notes.max_notes = 1).await;
    let chat = chat(&h).await;
    let now = h.clock.get();
    // Снимок мог быть создан до снижения max_notes в конфиге.
    let id = h
        .soul
        .shared()
        .db()
        .write(move |tx| -> DbResult<_> {
            for name in ["a", "b"] {
                tx.notes().insert(
                    chat,
                    &eva_soul::db::notes::NoteRow {
                        name: name.into(),
                        description: "old configuration".into(),
                        body: "body".into(),
                        prev_body: None,
                        pinned: false,
                        created_at: now,
                        updated_at: now,
                    },
                )?;
            }
            let id = tx.notes().checkpoint(chat, "save", now, 50)?;
            tx.notes().replace(chat, &[], now)?;
            Ok(id)
        })
        .await
        .unwrap();
    let before = history(&h, chat).await.snapshots;
    let result = h.client_session().await.notes().restore(chat, id).await;
    assert!(matches!(result, Err(Generic::Aux(Aux::Invalid(_)))));
    assert_eq!(history(&h, chat).await.snapshots, before);
    assert!(list(&h, chat).await.notes.is_empty());
}

#[tokio::test]
async fn disabled_notebook_rejects_owner_mutations_and_history() {
    let h = common::start_with(|cfg| cfg.notes.enabled = false).await;
    let chat = chat(&h).await;
    let mut owner = h.client_session().await;
    assert!(matches!(
        owner
            .notes()
            .inner_mut()
            .put()
            .call(args(chat, "a", "body"))
            .await
            .map(|ok| ok.0),
        Err(Generic::Aux(Aux::Unavailable(_)))
    ));
    assert!(matches!(
        owner.notes().delete(chat, "a".into()).await.map(|ok| ok.0),
        Err(Generic::Aux(Aux::Unavailable(_)))
    ));
    assert!(matches!(
        owner.notes().history(chat, None, Default::default()).await,
        Err(Generic::Aux(Aux::Unavailable(_)))
    ));
    assert!(matches!(
        owner.notes().snapshot(chat, 1).await,
        Err(Generic::Aux(Aux::Unavailable(_)))
    ));
    assert!(matches!(
        owner.notes().restore(chat, 1).await,
        Err(Generic::Aux(Aux::Unavailable(_)))
    ));
}
