//! `chats.*` M1: create, get, list, update, move, delete, restore,
//! history_read (`05 §2`, `§6`, `§7.3`, `§8`, `11 §3`).

mod common;

use std::{sync::atomic::Ordering, time::Duration};

use common::{Harness, OTHER, Outcome, SVC, TestSession, outcome};
use eva_core::{
    CallStep as _,
    service::{chats::Chats as _, folders::Folders as _},
};
use eva_protocol::{
    errors::Generic,
    requests::{
        Limit, Patch,
        chats::{HistoryCount, create, delete, get, history_read, list, r#move, restore, update},
        folders,
    },
    types::{
        ChatId, Effort, FolderId, PersonId, SurfaceId, TurnId,
        chat::{Chat, ExternalKind, Kind, Role, Selector},
        content::Block,
        folder,
        turn::{Audience, TurnView},
    },
    wire::events::Event,
};

use eva_kernel::{
    bus::Published,
    db::{Db as _, DbError},
};

fn create_args(surface: &str) -> create::Args {
    create::Args {
        surface: SurfaceId::from(surface),
        folder: None,
        external_id: None,
        external_kind: None,
        kind: None,
        title: None,
        project: None,
        workdir: None,
        workdir_on: None,
    }
}

async fn new_chat(session: &mut TestSession, args: create::Args) -> Chat {
    session
        .chats()
        .create()
        .call(args)
        .await
        .expect("create chat")
        .chat
}

async fn get(session: &mut TestSession, chat: ChatId) -> Chat {
    session
        .chats()
        .get()
        .call(get::Args { chat: chat.into() })
        .await
        .expect("get")
        .chat
}

async fn new_folder(session: &mut TestSession, surface: &str, name: &str) -> FolderId {
    session
        .folders()
        .create()
        .call(folders::create::Args {
            surface: SurfaceId::from(surface),
            name: String::from(name),
        })
        .await
        .expect("create folder")
        .folder
        .id
}

async fn set_folder_trust(session: &mut TestSession, folder: FolderId, trusted: Option<bool>) {
    session
        .folders()
        .update()
        .call(folders::update::Args {
            folder,
            update: folders::update::Update {
                trusted: Patch::Change(trusted),
                ..Default::default()
            },
        })
        .await
        .expect("folder trust");
}

async fn update(
    session: &mut TestSession,
    chat: ChatId,
    update: update::Update,
) -> Result<Chat, Generic<update::Err>> {
    session
        .chats()
        .update()
        .call(update::Args { chat, update })
        .await
        .map(|ok| ok.chat)
}

async fn exec(h: &Harness, sql: &'static str, chat: ChatId) {
    h.kernel
        .shared()
        .db()
        .write(move |tx| {
            tx.execute(sql, [chat.0])?;
            Ok::<_, DbError>(())
        })
        .await
        .expect("exec");
}

/// Открытый сегмент чата: без него сбросу нечего закрывать.
async fn open_segment(h: &Harness, chat: ChatId) -> i64 {
    h.kernel
        .shared()
        .db()
        .write(move |tx| {
            Ok::<i64, DbError>(tx.query_row(
                "INSERT INTO segments (chat_id, seq, started_at, reason, system_snapshot, tools_snapshot, hot_memory_ids)
                 VALUES (?1, 1, 1, 'initial', x'', x'', x'') RETURNING id",
                [chat.0],
                |r| r.get(0),
            )?)
        })
        .await
        .expect("segment")
}

/// Реплика в логе сегмента: событие `message` и его строка `messages`.
async fn append_message(h: &Harness, chat: ChatId, segment: i64, seq: i64, text: String) {
    let payload = rmp_serde::to_vec_named(&serde_json::json!({
        "role": "user",
        "speaker": null,
        "blocks": [Block::text(text)],
    }))
    .expect("payload");
    h.kernel
        .shared()
        .db()
        .write(move |tx| {
            let event: i64 = tx.query_row(
                "INSERT INTO events (chat_id, segment_id, seq, kind, payload, created_at)
                 VALUES (?1, ?2, ?3, 'message', ?4, ?3) RETURNING id",
                (chat.0, segment, seq, payload),
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO messages (chat_id, seq, event_id, role, sender_person_id, external_id)
                 VALUES (?1, ?2, ?3, 'user', 'p0', 'x' || ?2)",
                (chat.0, seq, event),
            )?;
            Ok::<_, DbError>(())
        })
        .await
        .expect("message");
}

async fn reset_pending(h: &Harness, chat: ChatId) -> bool {
    h.query(move |c| {
        Ok(c.query_row(
            "SELECT reset_pending IS NOT NULL FROM chats WHERE id = ?1",
            [chat.0],
            |r| r.get(0),
        )?)
    })
    .await
}

fn drain(rx: &mut tokio::sync::broadcast::Receiver<Published>) -> Vec<Published> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

#[tokio::test]
async fn create_defaults_and_get() {
    let h = common::start().await;
    let mut events = h.kernel.shared().bus().subscribe();
    let mut client = h.client_session().await;

    let chat = new_chat(&mut client, create_args("local")).await;
    assert_eq!(chat.surface_id.as_str(), "local");
    assert_eq!(chat.kind, Kind::Ephemeral, "chats.default_kind");
    assert_eq!(chat.trusted, None);
    assert!(chat.trusted_chat, "builtin local is trusted");
    assert_eq!(chat.alias, None);
    assert_eq!(chat.live_turn, None);
    assert!(!chat.pinned && !chat.alerted && !chat.undeletable);
    assert_eq!(chat.created_at, chat.last_activity_at);
    assert_eq!(get(&mut client, chat.id).await, chat);

    let published = drain(&mut events);
    assert!(
        matches!(published.as_slice(), [Published { event: Event::ChatCreated(e), surface: Some(s) }]
            if e.chat == chat.id && s.id.as_str() == "local"),
        "{published:?}"
    );

    let unsorted: String = h
        .query(move |c| {
            Ok(c.query_row(
                "SELECT name FROM folders WHERE id = ?1",
                [chat.folder_id.0],
                |r| r.get(0),
            )?)
        })
        .await;
    assert_eq!(unsorted, "Unsorted");
}

#[tokio::test]
async fn create_with_alias_and_everything() {
    let h = common::start_with(|cfg| cfg.chats.default_kind = Kind::Persistent).await;
    let _svc = h.service_with_surface(SVC).await;
    let mut client = h.client_session().await;
    let work = new_folder(&mut client, SVC, "work").await;

    let chat = new_chat(
        &mut client,
        create::Args {
            folder: Some(work),
            external_id: Some(String::from("-100")),
            external_kind: Some(ExternalKind::Group),
            title: Some(String::from("Team chat")),
            project: Some(String::from("eva")),
            workdir: Some(String::from("/work/eva")),
            workdir_on: Some(String::from(common::ENDPOINT)),
            ..create_args(SVC)
        },
    )
    .await;
    assert_eq!(chat.kind, Kind::Persistent, "configured default_kind");
    assert_eq!(chat.folder_id, work);
    assert!(!chat.trusted_chat, "new surface has trusted = NULL");
    let alias = chat.alias.clone().expect("alias");
    assert_eq!(alias.external_id, "-100");
    assert_eq!(alias.kind, ExternalKind::Group);
    assert_eq!(chat.title.as_deref(), Some("Team chat"));
    assert_eq!(chat.project.as_deref(), Some("eva"));
    assert_eq!(chat.workdir.as_deref(), Some("/work/eva"));
    assert_eq!(chat.workdir_on.as_deref(), Some(common::ENDPOINT));

    let get::Ok { chat: by_alias } = client
        .chats()
        .get()
        .call(get::Args {
            chat: Selector::Alias {
                surface: SurfaceId::from(SVC),
                external_id: String::from("-100"),
            },
        })
        .await
        .expect("get by alias");
    assert_eq!(by_alias, chat);

    let titles: Vec<String> = h
        .query(move |c| {
            let mut stmt =
                c.prepare("SELECT text FROM chat_speech WHERE chat_id = ?1 AND kind = 'title'")?;
            Ok(stmt
                .query_map([chat.id.0], |r| r.get(0))?
                .collect::<Result<_, _>>()?)
        })
        .await;
    assert_eq!(titles, ["Team chat"], "title row of the search index");

    let direct = new_chat(
        &mut client,
        create::Args {
            external_id: Some(String::from("7")),
            ..create_args(SVC)
        },
    )
    .await;
    assert_eq!(direct.alias.map(|a| a.kind), Some(ExternalKind::Direct));

    let res = client
        .chats()
        .create()
        .call(create::Args {
            external_id: Some(String::from("-100")),
            ..create_args(SVC)
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Conflict, "alias is taken: {res:?}");
}

#[tokio::test]
async fn create_rejects_bad_arguments() {
    let h = common::start().await;
    let _svc = h.service_with_surface(SVC).await;
    let mut client = h.client_session().await;
    let foreign_folder = new_folder(&mut client, "local", "work").await;

    let cases = [
        (create_args("nowhere"), Outcome::NotFound, "missing surface"),
        (
            create::Args {
                folder: Some(FolderId(9_999)),
                ..create_args(SVC)
            },
            Outcome::NotFound,
            "missing folder",
        ),
        (
            create::Args {
                folder: Some(foreign_folder),
                ..create_args(SVC)
            },
            Outcome::Invalid,
            "folder of another surface",
        ),
        (
            create::Args {
                external_kind: Some(ExternalKind::Group),
                ..create_args(SVC)
            },
            Outcome::Invalid,
            "external_kind without external_id",
        ),
        (
            create::Args {
                external_id: Some(String::new()),
                ..create_args(SVC)
            },
            Outcome::Invalid,
            "empty external_id",
        ),
        (
            create::Args {
                workdir_on: Some(String::from("laptop")),
                ..create_args(SVC)
            },
            Outcome::Invalid,
            "unknown hands endpoint",
        ),
        (
            create::Args {
                title: Some(String::from("a\nb")),
                ..create_args(SVC)
            },
            Outcome::Invalid,
            "multiline title",
        ),
    ];
    for (args, want, why) in cases {
        let res = client.chats().create().call(args).await;
        assert_eq!(outcome(&res), want, "{why}: {res:?}");
    }
}

#[tokio::test]
async fn service_creates_only_in_folders_trusted_like_unsorted() {
    let h = common::start().await;
    let mut svc = h.service_with_surface(SVC).await;
    let mut client = h.client_session().await;
    let trusted = new_folder(&mut client, SVC, "trusted").await;
    set_folder_trust(&mut client, trusted, Some(true)).await;
    let plain = new_folder(&mut client, SVC, "plain").await;
    set_folder_trust(&mut client, plain, Some(false)).await;

    new_chat(&mut svc, create_args(SVC)).await;
    let chat = new_chat(
        &mut svc,
        create::Args {
            folder: Some(plain),
            ..create_args(SVC)
        },
    )
    .await;
    assert!(!chat.trusted_chat);

    let res = svc
        .chats()
        .create()
        .call(create::Args {
            folder: Some(trusted),
            ..create_args(SVC)
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Forbidden, "{res:?}");

    let chat = new_chat(
        &mut client,
        create::Args {
            folder: Some(trusted),
            ..create_args(SVC)
        },
    )
    .await;
    assert!(chat.trusted_chat, "the owner may");
}

#[tokio::test]
async fn get_not_found_keeps_the_selector() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    let res = client
        .chats()
        .get()
        .call(get::Args {
            chat: ChatId(404).into(),
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(get::Err::NotFound(e))) if e.chat == Selector::Id(ChatId(404))),
        "{res:?}"
    );
    let alias = Selector::Alias {
        surface: SurfaceId::from("local"),
        external_id: String::from("nope"),
    };
    let res = client
        .chats()
        .get()
        .call(get::Args {
            chat: alias.clone(),
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(get::Err::NotFound(e))) if e.chat == alias),
        "{res:?}"
    );
}

#[tokio::test]
async fn list_filters_and_pages_by_activity() {
    let h = common::start().await;
    let mut svc = h.service_with_surface(SVC).await;
    let mut other = h.service_with_surface(OTHER).await;
    let mut client = h.client_session().await;
    let work = new_folder(&mut client, SVC, "work").await;
    let mut clock = h.clock.clone();

    let mut mine = Vec::new();
    for i in 0..5 {
        clock.advance(Duration::from_secs(60));
        let folder = (i % 2 == 0).then_some(work);
        mine.push(
            new_chat(
                &mut client,
                create::Args {
                    folder,
                    ..create_args(SVC)
                },
            )
            .await
            .id,
        );
    }
    new_chat(&mut other, create_args(OTHER)).await;
    new_chat(&mut client, create_args("local")).await;
    // Старый чат, чья активность свежее всех: порядок — по last_activity_at.
    exec(
        &h,
        "UPDATE chats SET last_activity_at = last_activity_at + 3600000000000 WHERE id = ?1",
        mine[0],
    )
    .await;

    let limit = Limit::new(2).expect("limit");
    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let list::Ok { found } = svc
            .chats()
            .list()
            .call(list::Args {
                limit,
                after,
                ..Default::default()
            })
            .await
            .expect("page");
        assert!(
            found.iter().all(|c| c.surface_id.as_str() == SVC),
            "only own"
        );
        let last = found.len() < usize::from(limit.into_inner());
        after = found.last().map(|c| c.id);
        seen.extend(found.into_iter().map(|c| c.id));
        if last {
            break;
        }
    }
    assert_eq!(seen, [mine[0], mine[4], mine[3], mine[2], mine[1]]);

    let list::Ok { found } = client
        .chats()
        .list()
        .call(list::Args {
            folder: Some(work),
            ..Default::default()
        })
        .await
        .expect("by folder");
    assert_eq!(found.len(), 3);

    let list::Ok { found } = client
        .chats()
        .list()
        .call(list::Args {
            surface: Some(SurfaceId::from(OTHER)),
            ..Default::default()
        })
        .await
        .expect("by surface");
    assert_eq!(found.len(), 1);

    let res = svc
        .chats()
        .list()
        .call(list::Args {
            surface: Some(SurfaceId::from(OTHER)),
            ..Default::default()
        })
        .await;
    assert_eq!(
        outcome(&res),
        Outcome::Forbidden,
        "a foreign filter: {res:?}"
    );

    let list::Ok { found } = client
        .chats()
        .list()
        .call(list::Args::default())
        .await
        .expect("all");
    assert_eq!(found.len(), 7);

    // Якорь страницы исчез.
    client
        .chats()
        .delete()
        .call(delete::Args {
            chat: mine[1],
            purge: true,
        })
        .await
        .expect("purge");
    let res = client
        .chats()
        .list()
        .call(list::Args {
            after: Some(mine[1]),
            ..Default::default()
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Conflict, "{res:?}");
}

#[tokio::test]
async fn list_live_and_deleted() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    let quiet = new_chat(&mut client, create_args("local")).await.id;
    let busy = new_chat(&mut client, create_args("local")).await.id;
    let gone = new_chat(&mut client, create_args("local")).await.id;

    let segment = open_segment(&h, busy).await;
    h.kernel
        .shared()
        .db()
        .write(move |tx| {
            tx.execute(
                "INSERT INTO events (chat_id, segment_id, seq, kind, payload, turn_id, created_at)
                 VALUES (?1, ?2, 1, 'reminder', x'80', 'turn-1', 1)",
                (busy.0, segment),
            )?;
            Ok::<_, DbError>(())
        })
        .await
        .expect("turn event");
    client
        .chats()
        .delete()
        .call(delete::Args {
            chat: gone,
            purge: false,
        })
        .await
        .expect("delete");

    let ids = |found: Vec<Chat>| found.into_iter().map(|c| c.id).collect::<Vec<_>>();
    let list::Ok { found } = client
        .chats()
        .list()
        .call(list::Args {
            live: true,
            ..Default::default()
        })
        .await
        .expect("live");
    assert_eq!(found[0].live_turn, Some(TurnId::from("turn-1")));
    assert_eq!(ids(found), [busy]);

    let list::Ok { found } = client
        .chats()
        .list()
        .call(list::Args::default())
        .await
        .expect("default");
    let mut listed = ids(found);
    listed.sort();
    assert_eq!(listed, [quiet, busy], "deleted chats are hidden");

    let list::Ok { found } = client
        .chats()
        .list()
        .call(list::Args {
            deleted: true,
            ..Default::default()
        })
        .await
        .expect("deleted");
    assert_eq!(ids(found), [gone]);

    h.kernel
        .shared()
        .db()
        .write(move |tx| {
            tx.execute(
                "INSERT INTO events (chat_id, segment_id, seq, kind, payload, turn_id, created_at)
                 VALUES (?1, ?2, 2, 'turn_ended', x'80', 'turn-1', 2)",
                (busy.0, segment),
            )?;
            Ok::<_, DbError>(())
        })
        .await
        .expect("turn ended");
    assert_eq!(get(&mut client, busy).await.live_turn, None);
}

#[tokio::test]
async fn update_fields_and_patch_semantics() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    let chat = new_chat(&mut client, create_args("local")).await.id;

    let updated = update(
        &mut client,
        chat,
        update::Update {
            title: Patch::Change(Some(String::from("Plans"))),
            alerted: Patch::Change(true),
            model: Patch::Change(Some(common::MODEL.into())),
            effort: Patch::Change(Some(Effort::Max)),
            compact_at: Patch::Change(Some(0.8)),
            project: Patch::Change(Some(String::from("eva"))),
            workdir: Patch::Change(Some(String::from("/src"))),
            workdir_on: Patch::Change(Some(String::from(common::ENDPOINT))),
            ..Default::default()
        },
    )
    .await
    .expect("update");
    assert_eq!(updated.title.as_deref(), Some("Plans"));
    assert!(updated.alerted);
    assert_eq!(updated.effort, Some(Effort::Max));
    assert_eq!(updated.compact_at, Some(0.8));
    assert_eq!(updated.workdir_on.as_deref(), Some(common::ENDPOINT));

    let keep: update::Update = serde_json::from_str("{}").expect("keep");
    assert_eq!(
        update(&mut client, chat, keep).await.expect("keep"),
        updated
    );

    let clear: update::Update =
        serde_json::from_str(r#"{"title": {"change": null}, "project": null}"#).expect("clear");
    let cleared = update(&mut client, chat, clear).await.expect("clear");
    assert_eq!(cleared.title, None);
    assert_eq!(cleared.project.as_deref(), Some("eva"), "null is keep");
    let titles: i64 = h
        .query(move |c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM chat_speech WHERE chat_id = ?1 AND kind = 'title'",
                [chat.0],
                |r| r.get(0),
            )?)
        })
        .await;
    assert_eq!(titles, 0, "cleared title leaves the index");

    for (bad, why) in [
        (
            update::Update {
                workdir_on: Patch::Change(Some(String::from("laptop"))),
                ..Default::default()
            },
            "unknown endpoint",
        ),
        (
            update::Update {
                compact_at: Patch::Change(Some(0.0)),
                ..Default::default()
            },
            "compact_at out of range",
        ),
        (
            update::Update {
                title: Patch::Change(Some(String::from("  "))),
                ..Default::default()
            },
            "blank title",
        ),
    ] {
        let res = update(&mut client, chat, bad).await;
        assert!(
            matches!(
                res,
                Err(Generic::Aux(eva_protocol::errors::Aux::Invalid(_)))
            ),
            "{why}: {res:?}"
        );
    }

    let res = update(&mut client, ChatId(404), update::Update::default()).await;
    assert!(
        matches!(&res, Err(Generic::Spec(update::Err::NotFound(_)))),
        "{res:?}"
    );
}

#[tokio::test]
async fn kind_is_monotonic_and_pin_persists() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    let chat = new_chat(&mut client, create_args("local")).await.id;

    let pinned = update(
        &mut client,
        chat,
        update::Update {
            pinned: Patch::Change(true),
            ..Default::default()
        },
    )
    .await
    .expect("pin");
    assert!(pinned.pinned);
    assert_eq!(
        pinned.kind,
        Kind::Persistent,
        "pin makes the chat persistent"
    );

    let unpinned = update(
        &mut client,
        chat,
        update::Update {
            pinned: Patch::Change(false),
            ..Default::default()
        },
    )
    .await
    .expect("unpin");
    assert_eq!(unpinned.kind, Kind::Persistent, "unpin keeps the kind");

    let res = update(
        &mut client,
        chat,
        update::Update {
            kind: Patch::Change(Kind::Ephemeral),
            ..Default::default()
        },
    )
    .await;
    assert!(
        matches!(&res, Err(Generic::Spec(update::Err::KindMonotonic(e))) if e.chat == chat),
        "{res:?}"
    );

    let other = new_chat(&mut client, create_args("local")).await.id;
    let persisted = update(
        &mut client,
        other,
        update::Update {
            kind: Patch::Change(Kind::Persistent),
            ..Default::default()
        },
    )
    .await
    .expect("persist");
    assert_eq!(persisted.kind, Kind::Persistent);
}

#[tokio::test]
async fn subagent_trust_is_a_snapshot() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    let lead = new_chat(&mut client, create_args("local")).await.id;
    let agent = new_chat(&mut client, create_args("local")).await.id;
    h.kernel
        .shared()
        .db()
        .write(move |tx| {
            tx.execute(
                "INSERT INTO agents (id, parent_chat_id, name, task, tools, master_turn, status, started_at)
                 VALUES (?1, ?2, 'scout', 'look around', '[]', 1, 'running', 1)",
                (agent.0, lead.0),
            )?;
            Ok::<_, DbError>(())
        })
        .await
        .expect("agent row");

    let res = update(
        &mut client,
        agent,
        update::Update {
            trusted: Patch::Change(Some(true)),
            ..Default::default()
        },
    )
    .await;
    assert!(
        matches!(&res, Err(Generic::Spec(update::Err::AgentTrust(e))) if e.chat == agent),
        "{res:?}"
    );
    update(
        &mut client,
        agent,
        update::Update {
            title: Patch::Change(Some(String::from("agent: scout"))),
            ..Default::default()
        },
    )
    .await
    .expect("other fields are fine");
}

#[tokio::test]
async fn service_edits_only_its_fields() {
    let h = common::start().await;
    let mut svc = h.service_with_surface(SVC).await;
    let chat = new_chat(&mut svc, create_args(SVC)).await.id;

    let edited = update(
        &mut svc,
        chat,
        update::Update {
            title: Patch::Change(Some(String::from("Group"))),
            pinned: Patch::Change(true),
            alerted: Patch::Change(true),
            project: Patch::Change(Some(String::from("p"))),
            workdir: Patch::Change(Some(String::from("/w"))),
            workdir_on: Patch::Change(Some(String::from(common::ENDPOINT))),
            ..Default::default()
        },
    )
    .await
    .expect("service fields");
    assert!(edited.pinned);

    for trust_field in [
        update::Update {
            trusted: Patch::Change(Some(true)),
            ..Default::default()
        },
        update::Update {
            model: Patch::Change(None),
            ..Default::default()
        },
        update::Update {
            effort: Patch::Change(Some(Effort::Low)),
            ..Default::default()
        },
        update::Update {
            thinking: Patch::Change(None),
            ..Default::default()
        },
        update::Update {
            compact_at: Patch::Change(Some(0.5)),
            ..Default::default()
        },
    ] {
        let res = update(&mut svc, chat, trust_field).await;
        assert_eq!(outcome(&res), Outcome::Forbidden, "{res:?}");
    }
}

#[tokio::test]
async fn trust_change_resets_the_open_segment() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    let chat = new_chat(&mut client, create_args("local")).await.id;

    update(
        &mut client,
        chat,
        update::Update {
            trusted: Patch::Change(Some(false)),
            ..Default::default()
        },
    )
    .await
    .expect("no segment yet");
    assert!(!reset_pending(&h, chat).await, "nothing to reset");

    open_segment(&h, chat).await;
    let same = update(
        &mut client,
        chat,
        update::Update {
            trusted: Patch::Change(Some(false)),
            ..Default::default()
        },
    )
    .await
    .expect("same trust");
    assert!(!same.trusted_chat);
    assert!(!reset_pending(&h, chat).await, "effective trust unchanged");

    let flipped = update(
        &mut client,
        chat,
        update::Update {
            trusted: Patch::Change(None),
            ..Default::default()
        },
    )
    .await
    .expect("back to inherited");
    assert!(flipped.trusted_chat, "local is trusted");
    assert!(reset_pending(&h, chat).await);
}

#[tokio::test]
async fn move_within_surface_with_trust_confirmation() {
    let h = common::start().await;
    let _svc = h.service_with_surface(SVC).await;
    let mut client = h.client_session().await;
    let chat = new_chat(&mut client, create_args("local")).await.id;
    open_segment(&h, chat).await;
    let same_trust = new_folder(&mut client, "local", "work").await;
    let untrusted = new_folder(&mut client, "local", "public").await;
    set_folder_trust(&mut client, untrusted, Some(false)).await;
    let foreign = new_folder(&mut client, SVC, "work").await;

    let r#move::Ok { chat: moved } = client
        .chats()
        .r#move()
        .call(r#move::Args {
            chat,
            folder: same_trust.into(),
            confirm: false,
        })
        .await
        .expect("same trust");
    assert_eq!(moved.folder_id, same_trust);
    assert!(!reset_pending(&h, chat).await);

    let by_name = folder::Selector::Name {
        surface: SurfaceId::from("local"),
        name: String::from("public"),
    };
    let res = client
        .chats()
        .r#move()
        .call(r#move::Args {
            chat,
            folder: by_name.clone(),
            confirm: false,
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(r#move::Err::TrustChange(e))) if e.chat == chat && e.from && !e.to),
        "{res:?}"
    );
    assert_eq!(
        get(&mut client, chat).await.folder_id,
        same_trust,
        "not moved"
    );

    let r#move::Ok { chat: moved } = client
        .chats()
        .r#move()
        .call(r#move::Args {
            chat,
            folder: by_name,
            confirm: true,
        })
        .await
        .expect("confirmed");
    assert_eq!(moved.folder_id, untrusted);
    assert!(!moved.trusted_chat);
    assert!(reset_pending(&h, chat).await, "segment_reset is pending");

    let res = client
        .chats()
        .r#move()
        .call(r#move::Args {
            chat,
            folder: foreign.into(),
            confirm: true,
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(r#move::Err::CrossSurface(e))) if e.surface.as_str() == "local"),
        "{res:?}"
    );

    let res = client
        .chats()
        .r#move()
        .call(r#move::Args {
            chat,
            folder: FolderId(9_999).into(),
            confirm: true,
        })
        .await;
    assert_eq!(outcome(&res), Outcome::NotFound, "missing folder: {res:?}");
    let res = client
        .chats()
        .r#move()
        .call(r#move::Args {
            chat: ChatId(404),
            folder: same_trust.into(),
            confirm: true,
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(r#move::Err::NotFound(_)))),
        "{res:?}"
    );
}

#[tokio::test]
async fn explicit_chat_trust_survives_a_move() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    let chat = new_chat(&mut client, create_args("local")).await.id;
    update(
        &mut client,
        chat,
        update::Update {
            trusted: Patch::Change(Some(true)),
            ..Default::default()
        },
    )
    .await
    .expect("explicit trust");
    let untrusted = new_folder(&mut client, "local", "public").await;
    set_folder_trust(&mut client, untrusted, Some(false)).await;

    let r#move::Ok { chat: moved } = client
        .chats()
        .r#move()
        .call(r#move::Args {
            chat,
            folder: untrusted.into(),
            confirm: false,
        })
        .await
        .expect("chat trust wins over the folder");
    assert!(moved.trusted_chat);
}

#[tokio::test]
async fn delete_restore_and_purge() {
    let h = common::start().await;
    let mut events = h.kernel.shared().bus().subscribe();
    let mut client = h.client_session().await;
    let chat = new_chat(
        &mut client,
        create::Args {
            external_id: Some(String::from("ext")),
            ..create_args("local")
        },
    )
    .await
    .id;
    drain(&mut events);

    client
        .chats()
        .delete()
        .call(delete::Args { chat, purge: false })
        .await
        .expect("delete");
    let deleted = get(&mut client, chat).await;
    assert!(deleted.deleted_at.is_some());
    assert!(deleted.alias.is_some(), "alias stays for auto-restore");
    let published = drain(&mut events);
    assert!(
        matches!(published.as_slice(), [Published { event: Event::ChatDeleted(e), .. }]
            if e.chat == chat && e.external_id.as_deref() == Some("ext")),
        "{published:?}"
    );

    client
        .chats()
        .delete()
        .call(delete::Args { chat, purge: false })
        .await
        .expect("repeat delete");
    assert_eq!(get(&mut client, chat).await.deleted_at, deleted.deleted_at);
    drain(&mut events);

    let restore::Ok { chat: restored } = client
        .chats()
        .restore()
        .call(restore::Args { chat })
        .await
        .expect("restore");
    assert_eq!(restored.deleted_at, None);
    assert!(matches!(
        drain(&mut events).as_slice(),
        [Published {
            event: Event::ChatRestored(_),
            ..
        }]
    ));

    let res = client.chats().restore().call(restore::Args { chat }).await;
    assert!(
        matches!(&res, Err(Generic::Spec(restore::Err::NotDeleted(e))) if e.chat == chat),
        "{res:?}"
    );
    let res = client
        .chats()
        .restore()
        .call(restore::Args { chat: ChatId(404) })
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(restore::Err::NotFound(_)))),
        "{res:?}"
    );

    // Purge уносит чат, его подагентов и alias.
    let agent = new_chat(&mut client, create_args("local")).await.id;
    let segment = open_segment(&h, chat).await;
    append_message(&h, chat, segment, 1, String::from("hi")).await;
    h.kernel
        .shared()
        .db()
        .write(move |tx| {
            tx.execute(
                "INSERT INTO agents (id, parent_chat_id, name, task, tools, master_turn, status, started_at)
                 VALUES (?1, ?2, 'scout', 't', '[]', 1, 'done', 1)",
                (agent.0, chat.0),
            )?;
            Ok::<_, DbError>(())
        })
        .await
        .expect("agent");
    drain(&mut events);

    client
        .chats()
        .delete()
        .call(delete::Args { chat, purge: true })
        .await
        .expect("purge");
    for id in [chat, agent] {
        let res = client
            .chats()
            .get()
            .call(get::Args { chat: id.into() })
            .await;
        assert!(
            matches!(&res, Err(Generic::Spec(get::Err::NotFound(_)))),
            "{id} is gone: {res:?}"
        );
    }
    let leftovers: i64 = h
        .query(|c| {
            Ok(c.query_row(
                "SELECT (SELECT COUNT(*) FROM chat_aliases) + (SELECT COUNT(*) FROM events)
                      + (SELECT COUNT(*) FROM messages) + (SELECT COUNT(*) FROM agents)",
                [],
                |r| r.get(0),
            )?)
        })
        .await;
    assert_eq!(leftovers, 0);
    let published = drain(&mut events);
    assert!(
        matches!(published.as_slice(), [Published { event: Event::ChatPurged(e), surface: Some(s) }]
            if e.ids == [chat, agent] && s.id.as_str() == "local"),
        "{published:?}"
    );

    let res = client
        .chats()
        .delete()
        .call(delete::Args {
            chat: ChatId(404),
            purge: false,
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(delete::Err::NotFound(_)))),
        "{res:?}"
    );
}

#[tokio::test]
async fn undeletable_refuses_both_deletes() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    let chat = new_chat(&mut client, create_args("system")).await.id;
    exec(
        &h,
        "UPDATE chats SET kind = 'persistent', undeletable = 1 WHERE id = ?1",
        chat,
    )
    .await;

    for purge in [false, true] {
        let res = client
            .chats()
            .delete()
            .call(delete::Args { chat, purge })
            .await;
        assert!(
            matches!(&res, Err(Generic::Spec(delete::Err::Undeletable(e))) if e.chat == chat),
            "purge={purge}: {res:?}"
        );
    }
    assert_eq!(get(&mut client, chat).await.deleted_at, None);
}

#[tokio::test]
async fn history_reads_in_pages() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    let chat = new_chat(&mut client, create_args("local")).await.id;
    let segment = open_segment(&h, chat).await;
    for seq in 1..=5 {
        append_message(&h, chat, segment, seq, format!("line {seq}")).await;
    }

    let read = |from_seq, count| history_read::Args {
        before: None,
        chat,
        from_seq,
        count: HistoryCount::new(count).expect("count"),
        view: None,
    };
    let history_read::Ok { messages, next_seq } = client
        .chats()
        .history_read()
        .call(read(0, 2))
        .await
        .expect("first page");
    assert_eq!(messages.iter().map(|m| m.seq).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(next_seq, Some(3));
    let first = &messages[0];
    assert_eq!(first.role, Role::User);
    assert_eq!(first.sender, Some(PersonId::system()));
    assert_eq!(first.blocks, [Block::text("line 1")]);
    assert_eq!(first.external_id.as_deref(), Some("x1"));

    let history_read::Ok { messages, next_seq } = client
        .chats()
        .history_read()
        .call(read(3, 3))
        .await
        .expect("exact tail");
    assert_eq!(
        messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
        [3, 4, 5]
    );
    assert_eq!(next_seq, None, "read to the end");

    let history_read::Ok { messages, next_seq } = client
        .chats()
        .history_read()
        .call(read(6, 10))
        .await
        .expect("past the end");
    assert!(messages.is_empty());
    assert_eq!(next_seq, None);

    let res = client
        .chats()
        .history_read()
        .call(history_read::Args {
            before: None,
            chat: ChatId(404),
            ..read(0, 1)
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(history_read::Err::NotFound(_)))),
        "{res:?}"
    );
}

#[tokio::test]
async fn history_respects_the_turn_view() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    let current = new_chat(&mut client, create_args("local")).await.id;
    let trusted = new_chat(&mut client, create_args("local")).await.id;
    let untrusted = new_chat(&mut client, create_args("local")).await.id;
    update(
        &mut client,
        untrusted,
        update::Update {
            trusted: Patch::Change(Some(false)),
            ..Default::default()
        },
    )
    .await
    .expect("untrusted");

    let view = |master_turn, trusted_chat| TurnView {
        chat: current,
        actor: None,
        master_turn,
        trusted_chat,
        audience: Audience::Mixed,
    };
    let read = |chat, view| history_read::Args {
        before: None,
        chat,
        from_seq: 0,
        count: HistoryCount::default(),
        view: Some(view),
    };

    for (chat, view, want) in [
        (trusted, view(true, true), Outcome::Ok),
        (trusted, view(false, true), Outcome::Forbidden),
        (trusted, view(true, false), Outcome::Forbidden),
        (current, view(false, false), Outcome::Ok),
        (untrusted, view(false, true), Outcome::Ok),
    ] {
        let res = client.chats().history_read().call(read(chat, view)).await;
        assert_eq!(outcome(&res), want, "{chat}: {res:?}");
    }

    client
        .chats()
        .delete()
        .call(delete::Args {
            chat: untrusted,
            purge: false,
        })
        .await
        .expect("delete");
    let res = client
        .chats()
        .history_read()
        .call(read(untrusted, view(false, false)))
        .await;
    assert_eq!(
        outcome(&res),
        Outcome::Forbidden,
        "deleted is out of scope: {res:?}"
    );
}

#[tokio::test]
async fn database_failure_is_aux_db() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    let chat = new_chat(&mut client, create_args("local")).await.id;
    h.db_fail.store(true, Ordering::Relaxed);
    let res = client
        .chats()
        .get()
        .call(get::Args { chat: chat.into() })
        .await;
    assert_eq!(outcome(&res), Outcome::Db, "{res:?}");
    let res = update(&mut client, chat, update::Update::default()).await;
    assert_eq!(outcome(&res), Outcome::Db, "{res:?}");
}

/// Сервис не раздувает базу ядра строками до `max_frame`: заголовок, alias и
/// пути чата ограничены (`handling::text`).
#[tokio::test]
async fn oversized_strings_are_invalid() {
    let h = common::start().await;
    let mut svc = h.service_with_surface(SVC).await;
    let huge = "x".repeat(100_000);

    let cases = [
        (
            "title",
            create::Args {
                title: Some(huge.clone()),
                ..create_args(SVC)
            },
        ),
        (
            "external_id",
            create::Args {
                external_id: Some(huge.clone()),
                ..create_args(SVC)
            },
        ),
        (
            "project",
            create::Args {
                project: Some(huge.clone()),
                ..create_args(SVC)
            },
        ),
        (
            "workdir",
            create::Args {
                workdir: Some(huge.clone()),
                ..create_args(SVC)
            },
        ),
    ];
    for (what, args) in cases {
        let res = svc.chats().create().call(args).await;
        assert_eq!(outcome(&res), Outcome::Invalid, "create {what}: {res:?}");
    }

    let chat = new_chat(&mut svc, create_args(SVC)).await.id;
    let res = svc
        .chats()
        .update()
        .call(update::Args {
            chat,
            update: update::Update {
                title: Patch::Change(Some("t".repeat(257))),
                ..Default::default()
            },
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Invalid, "update title: {res:?}");
    let res = svc
        .chats()
        .update()
        .call(update::Args {
            chat,
            update: update::Update {
                title: Patch::Change(Some("т".repeat(256))),
                workdir: Patch::Change(Some(huge)),
                ..Default::default()
            },
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Invalid, "update workdir: {res:?}");

    let renamed = svc
        .chats()
        .update()
        .call(update::Args {
            chat,
            update: update::Update {
                title: Patch::Change(Some("т".repeat(256))),
                ..Default::default()
            },
        })
        .await
        .expect("256 characters fit, whatever their bytes")
        .chat;
    assert_eq!(renamed.title.map(|t| t.chars().count()), Some(256));
}
