//! `folders.*` (`05 §1.2`, `11 §3`).

mod common;

use std::sync::atomic::Ordering;

use common::{Outcome, SVC, TestSession, outcome};
use eva_core::{CallStep as _, service::Folders::Trait as _};
use eva_protocol::{
    errors::{Generic, folders::NameTaken},
    requests::{
        Limit, Patch, chats,
        folders::{create, delete, list, update},
    },
    types::{ChatId, Effort, FolderId, ModelId, SurfaceId, Thinking, folder::Folder},
};

async fn create(session: &mut TestSession, surface: &str, name: &str) -> Folder {
    session
        .folders()
        .create(SurfaceId::from(surface), String::from(name))
        .await
        .expect("create folder")
        .folder
}

async fn list_names(session: &mut TestSession, surface: &str) -> Vec<String> {
    let list::Ok { found } = session
        .folders()
        .inner_mut()
        .list()
        .call(list::Args {
            surface: Some(SurfaceId::from(surface)),
            ..Default::default()
        })
        .await
        .expect("list");
    found.into_iter().map(|f| f.name).collect()
}

fn update_args(folder: FolderId, update: update::Update) -> update::Args {
    update::Args { folder, update }
}

#[tokio::test]
async fn create_update_and_list() {
    let h = common::start().await;
    let mut client = h.client_session().await;

    let work = create(&mut client, "local", "work").await;
    assert_eq!(work.surface_id.as_str(), "local");
    assert_eq!(work.name, "work");
    assert!(!work.builtin);
    assert_eq!(work.trusted, None);

    assert_eq!(
        list_names(&mut client, "local").await,
        ["work", "agents", "Unsorted"],
        "newest first"
    );

    let update::Ok { folder } = client
        .folders()
        .inner_mut()
        .update()
        .call(update_args(
            work.id,
            update::Update {
                name: Patch::Change(String::from("projects")),
                trusted: Patch::Change(Some(false)),
                model: Patch::Change(Some(ModelId::from(common::MODEL))),
                effort: Patch::Change(Some(Effort::High)),
                thinking: Patch::Change(Some(Thinking::Off)),
                compact_at: Patch::Change(Some(0.5)),
            },
        ))
        .await
        .expect("update");
    assert_eq!(folder.name, "projects");
    assert_eq!(folder.trusted, Some(false));
    assert_eq!(
        folder.model.as_ref().map(ModelId::as_str),
        Some(common::MODEL)
    );
    assert_eq!(folder.effort, Some(Effort::High));
    assert_eq!(folder.thinking, Some(Thinking::Off));
    assert_eq!(folder.compact_at, Some(0.5));

    // `{}` — ничего не трогает, `{"change": null}` — очищает.
    let keep: update::Update = serde_json::from_str("{}").expect("keep");
    let update::Ok { folder: kept } = client
        .folders()
        .inner_mut()
        .update()
        .call(update_args(work.id, keep))
        .await
        .expect("keep update");
    assert_eq!(kept, folder);

    let clear: update::Update =
        serde_json::from_str(r#"{"trusted": {"change": null}, "model": null}"#).expect("clear");
    let update::Ok { folder: cleared } = client
        .folders()
        .inner_mut()
        .update()
        .call(update_args(work.id, clear))
        .await
        .expect("clear update");
    assert_eq!(cleared.trusted, None);
    assert_eq!(cleared.model, folder.model, "null is keep");
}

#[tokio::test]
async fn builtin_folders_keep_their_names() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    let list::Ok { found } = client
        .folders()
        .inner_mut()
        .list()
        .call(list::Args {
            surface: Some(SurfaceId::from("system")),
            ..Default::default()
        })
        .await
        .expect("list");
    assert!(found.iter().all(|f| f.builtin));
    let unsorted = found
        .iter()
        .find(|f| f.name == "Unsorted")
        .expect("Unsorted");

    let res = client
        .folders()
        .inner_mut()
        .update()
        .call(update_args(
            unsorted.id,
            update::Update {
                name: Patch::Change(String::from("Inbox")),
                ..Default::default()
            },
        ))
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(update::Err::Builtin(e))) if e.folder == unsorted.id),
        "{res:?}"
    );

    // Trust и модель builtin-папки правятся; тот же name — не переименование.
    let update::Ok { folder } = client
        .folders()
        .inner_mut()
        .update()
        .call(update_args(
            unsorted.id,
            update::Update {
                name: Patch::Change(String::from("Unsorted")),
                trusted: Patch::Change(Some(false)),
                ..Default::default()
            },
        ))
        .await
        .expect("builtin trust");
    assert_eq!(folder.trusted, Some(false));

    let res = client.folders().delete(unsorted.id).await;
    assert!(
        matches!(&res, Err(Generic::Spec(delete::Err::Builtin(e))) if e.folder == unsorted.id),
        "{res:?}"
    );
}

#[tokio::test]
async fn names_are_unique_per_surface() {
    let h = common::start().await;
    let _svc = h.service_with_surface(SVC).await;
    let mut client = h.client_session().await;

    let work = create(&mut client, "local", "work").await;
    create(&mut client, SVC, "work").await;

    let res = client
        .folders()
        .create(SurfaceId::from("local"), String::from("work"))
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(create::Err::NameTaken(NameTaken { surface, name })))
            if surface.as_str() == "local" && name == "work"),
        "{res:?}"
    );

    let res = client
        .folders()
        .inner_mut()
        .update()
        .call(update_args(
            work.id,
            update::Update {
                name: Patch::Change(String::from("agents")),
                ..Default::default()
            },
        ))
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(update::Err::NameTaken(_)))),
        "{res:?}"
    );
}

#[tokio::test]
async fn invalid_arguments_and_missing_objects() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    let work = create(&mut client, "local", "work").await;

    for name in ["", "  ", "two\nlines"] {
        let res = client
            .folders()
            .create(SurfaceId::from("local"), String::from(name))
            .await;
        assert_eq!(outcome(&res), Outcome::Invalid, "{name:?}: {res:?}");
    }

    let res = client
        .folders()
        .create(SurfaceId::from("nowhere"), String::from("x"))
        .await;
    assert_eq!(outcome(&res), Outcome::NotFound, "{res:?}");

    let res = client
        .folders()
        .inner_mut()
        .update()
        .call(update_args(
            work.id,
            update::Update {
                compact_at: Patch::Change(Some(1.5)),
                ..Default::default()
            },
        ))
        .await;
    assert_eq!(outcome(&res), Outcome::Invalid, "{res:?}");

    let ghost = FolderId(9_999);
    let res = client
        .folders()
        .inner_mut()
        .update()
        .call(update_args(ghost, update::Update::default()))
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(update::Err::NotFound(e))) if e.folder == ghost),
        "{res:?}"
    );
    let res = client.folders().delete(ghost).await;
    assert!(
        matches!(&res, Err(Generic::Spec(delete::Err::NotFound(e))) if e.folder == ghost),
        "{res:?}"
    );
}

#[tokio::test]
async fn delete_moves_chats_to_unsorted() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    let work = create(&mut client, "local", "work").await;

    let mut ids = Vec::new();
    for _ in 0..3 {
        let chats::create::Ok { chat } = client
            .chats()
            .create(
                SurfaceId::from("local"),
                Some(work.id),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .expect("chat in work");
        ids.push(chat.id);
    }
    // Удалённый чат папки тоже переезжает: иначе FK держал бы папку.
    client
        .chats()
        .delete(ids[0], false)
        .await
        .expect("soft delete");

    let delete::Ok { moved } = client.folders().delete(work.id).await.expect("delete");
    assert_eq!(moved, 3);
    assert!(
        !list_names(&mut client, "local")
            .await
            .contains(&String::from("work"))
    );

    for id in ids {
        let chats::get::Ok { chat } = client.chats().get(id.into()).await.expect("get");
        let folder: String = h
            .query(move |c| {
                Ok(c.query_row(
                    "SELECT name FROM folders WHERE id = ?1",
                    [chat.folder_id.0],
                    |r| r.get(0),
                )?)
            })
            .await;
        assert_eq!(folder, "Unsorted");
    }
}

#[tokio::test]
async fn list_pages_by_id() {
    let h = common::start().await;
    let _svc = h.service_with_surface(SVC).await;
    let mut client = h.client_session().await;
    for i in 0..5 {
        create(&mut client, SVC, &format!("f{i}")).await;
    }
    // 2 builtin + 5 своих = 7 папок surface.
    let limit = Limit::new(3).expect("limit");
    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let list::Ok { found } = client
            .folders()
            .list(Some(SurfaceId::from(SVC)), limit, after)
            .await
            .expect("page");
        assert!(found.windows(2).all(|w| w[0].id > w[1].id), "id DESC");
        assert!(found.iter().all(|f| f.surface_id.as_str() == SVC));
        let last = found.len() < usize::from(limit.into_inner());
        after = found.last().map(|f| f.id);
        seen.extend(found.into_iter().map(|f| f.id));
        if last {
            break;
        }
    }
    assert_eq!(seen.len(), 7);
    let mut unique = seen.clone();
    unique.dedup();
    assert_eq!(unique, seen, "no duplicates across pages");

    let list::Ok { found } = client
        .folders()
        .list(Some(SurfaceId::from(SVC)), limit, seen.last().copied())
        .await
        .expect("past the end");
    assert!(found.is_empty());

    let list::Ok { found } = client
        .folders()
        .inner_mut()
        .list()
        .call(list::Args::default())
        .await
        .expect("all surfaces");
    assert!(found.iter().any(|f| f.surface_id.as_str() == "system"));
}

#[tokio::test]
async fn database_failure_is_aux_db() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    h.db_fail.store(true, Ordering::Relaxed);
    let res = client
        .folders()
        .inner_mut()
        .list()
        .call(list::Args::default())
        .await;
    assert_eq!(outcome(&res), Outcome::Db, "{res:?}");
    let res = client
        .folders()
        .create(SurfaceId::from("local"), String::from("x"))
        .await;
    assert_eq!(outcome(&res), Outcome::Db, "{res:?}");
}

async fn chat_in(session: &mut TestSession, folder: FolderId, trusted: Option<bool>) -> ChatId {
    let chat = session
        .chats()
        .create(
            SurfaceId::from("local"),
            Some(folder),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("chat in folder")
        .chat
        .id;
    if trusted.is_some() {
        session
            .chats()
            .update(
                chat,
                chats::update::Update {
                    trusted: Patch::Change(trusted),
                    ..Default::default()
                },
            )
            .await
            .expect("explicit chat trust");
    }
    chat
}

async fn set_trust(session: &mut TestSession, folder: FolderId, trusted: Option<bool>) -> Folder {
    session
        .folders()
        .inner_mut()
        .update()
        .call(update_args(
            folder,
            update::Update {
                trusted: Patch::Change(trusted),
                ..Default::default()
            },
        ))
        .await
        .expect("folder trust")
        .folder
}

/// Удаление папки переносит её чаты в `Unsorted`: сменивший trust чат
/// начинает новый сегмент, как после `chats.move` (`05 §2.3`).
#[tokio::test]
async fn delete_across_a_trust_boundary_resets_the_segment() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    let public = create(&mut client, "local", "public").await.id;
    set_trust(&mut client, public, Some(false)).await;

    let inherited = chat_in(&mut client, public, None).await;
    let explicit = chat_in(&mut client, public, Some(false)).await;
    let idle = chat_in(&mut client, public, None).await;
    h.open_segment(inherited).await;
    h.open_segment(explicit).await;

    let delete::Ok { moved } = client.folders().delete(public).await.expect("delete");
    assert_eq!(moved, 3);
    assert!(
        h.reset_pending(inherited).await,
        "untrusted → trusted Unsorted"
    );
    assert!(!h.reset_pending(explicit).await, "own trust did not change");
    assert!(!h.reset_pending(idle).await, "no open segment to reset");
    assert_eq!(
        h.counter("chats.trust_moves").await,
        2,
        "both inheriting chats changed trust"
    );
}

/// Правка `trusted` папки меняет trust наследующих чатов — тот же сброс.
#[tokio::test]
async fn folder_trust_change_resets_inheriting_chats() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    let team = create(&mut client, "local", "team").await.id;
    let inherited = chat_in(&mut client, team, None).await;
    let explicit = chat_in(&mut client, team, Some(true)).await;
    h.open_segment(inherited).await;
    h.open_segment(explicit).await;

    set_trust(&mut client, team, Some(true)).await;
    assert!(
        !h.reset_pending(inherited).await,
        "local is trusted already: nothing crossed"
    );

    let folder = set_trust(&mut client, team, Some(false)).await;
    assert_eq!(folder.trusted, Some(false));
    assert!(h.reset_pending(inherited).await);
    assert!(!h.reset_pending(explicit).await, "own trust wins");
    assert_eq!(h.counter("chats.trust_moves").await, 0, "no chat was moved");
}
