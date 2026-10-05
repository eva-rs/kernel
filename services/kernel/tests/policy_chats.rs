//! Матрица `Policy` (`11 §8`) для `chats.*`: surface чата решает, `Service`
//! видит только свои; жизненный цикл — только хозяину.

mod common;

use common::{OTHER, Outcome, SVC, TestSession, outcome};
use eva_core::{
    CallStep as _,
    service::{chats::Chats as _, folders::Folders as _},
};
use eva_protocol::{
    requests::{
        Patch,
        chats::{
            Selector, compact, create, delete, get, history_read, list, r#move, notify, restore,
            search, update,
        },
        folders,
    },
    types::{ChatId, FolderId, SurfaceId, folder},
};

use Outcome::{Forbidden as F, Ok as K, Unavailable as U};

#[derive(Debug, Clone, Copy)]
enum Case {
    CreateA,
    CreateLocal,
    GetA,
    GetLocal,
    List,
    UpdateTitleA,
    UpdateTrustA,
    UpdateTitleLocal,
    MoveA,
    DeleteA,
    RestoreA,
    HistoryA,
    HistorySystem,
    Search,
    Compact,
    Notify,
}

/// Ожидание по caller'ам: kernel, client, service-own (A), service-foreign (B).
const MATRIX: &[(Case, [Outcome; 4])] = &[
    (Case::CreateA, [K, K, K, F]),
    (Case::CreateLocal, [K, K, F, F]),
    (Case::GetA, [K, K, K, F]),
    (Case::GetLocal, [K, K, F, F]),
    (Case::List, [K, K, K, K]),
    (Case::UpdateTitleA, [K, K, K, F]),
    (Case::UpdateTrustA, [K, K, F, F]),
    (Case::UpdateTitleLocal, [K, K, F, F]),
    (Case::MoveA, [K, K, F, F]),
    (Case::DeleteA, [K, K, F, F]),
    (Case::RestoreA, [K, K, F, F]),
    (Case::HistoryA, [K, K, K, F]),
    (Case::HistorySystem, [K, K, F, F]),
    (Case::Search, [K, K, F, F]),
    (Case::Compact, [U, U, F, F]),
    (Case::Notify, [K, F, F, F]),
];

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

/// Свежие чаты на каждый прогон: `delete` не должен влиять на соседей.
struct Targets {
    a: ChatId,
    local: ChatId,
    system: ChatId,
    deleted_a: ChatId,
}

async fn targets(owner: &mut TestSession) -> Targets {
    let mut chat = async |surface: &str| {
        owner
            .chats()
            .create()
            .call(create_args(surface))
            .await
            .expect("target chat")
            .chat
            .id
    };
    let (a, local, system, deleted_a) = (
        chat(SVC).await,
        chat("local").await,
        chat("system").await,
        chat(SVC).await,
    );
    owner
        .chats()
        .delete()
        .call(delete::Args {
            chat: deleted_a,
            purge: false,
        })
        .await
        .expect("deleted target");
    Targets {
        a,
        local,
        system,
        deleted_a,
    }
}

fn title(chat: ChatId) -> update::Args {
    update::Args {
        chat,
        update: update::Update {
            title: Patch::Change(Some(String::from("Renamed"))),
            ..Default::default()
        },
    }
}

fn history(chat: ChatId) -> history_read::Args {
    history_read::Args {
        before: None,
        chat,
        from_seq: 0,
        count: Default::default(),
        view: None,
    }
}

async fn run(session: &mut TestSession, case: Case, t: &Targets) -> Outcome {
    let mut chats = session.chats();
    match case {
        Case::CreateA => outcome(&chats.create().call(create_args(SVC)).await),
        Case::CreateLocal => outcome(&chats.create().call(create_args("local")).await),
        Case::GetA => outcome(&chats.get().call(get::Args { chat: t.a.into() }).await),
        Case::GetLocal => outcome(
            &chats
                .get()
                .call(get::Args {
                    chat: t.local.into(),
                })
                .await,
        ),
        Case::List => outcome(&chats.list().call(list::Args::default()).await),
        Case::UpdateTitleA => outcome(&chats.update().call(title(t.a)).await),
        Case::UpdateTrustA => outcome(
            &chats
                .update()
                .call(update::Args {
                    chat: t.a,
                    update: update::Update {
                        trusted: Patch::Change(Some(true)),
                        ..Default::default()
                    },
                })
                .await,
        ),
        Case::UpdateTitleLocal => outcome(&chats.update().call(title(t.local)).await),
        Case::MoveA => outcome(
            &chats
                .r#move()
                .call(r#move::Args {
                    chat: t.a,
                    folder: folder::Selector::Name {
                        surface: SurfaceId::from(SVC),
                        name: String::from("agents"),
                    },
                    confirm: true,
                })
                .await,
        ),
        Case::DeleteA => outcome(
            &chats
                .delete()
                .call(delete::Args {
                    chat: t.a,
                    purge: false,
                })
                .await,
        ),
        Case::RestoreA => outcome(
            &chats
                .restore()
                .call(restore::Args { chat: t.deleted_a })
                .await,
        ),
        Case::HistoryA => outcome(&chats.history_read().call(history(t.a)).await),
        Case::HistorySystem => outcome(&chats.history_read().call(history(t.system)).await),
        Case::Search => outcome(
            &chats
                .search()
                .call(search::Args {
                    query: String::from("tea"),
                    limit: Default::default(),
                    since: None,
                    surface: None,
                    folder: None,
                    include_deleted: false,
                    view: None,
                })
                .await,
        ),
        Case::Compact => outcome(&chats.compact().call(compact::Args { chat: t.a }).await),
        Case::Notify => outcome(
            &chats
                .notify()
                .call(notify::Args {
                    subject: String::from("hi"),
                    body: String::from("body"),
                    source_chat: None,
                })
                .await,
        ),
    }
}

#[tokio::test]
async fn chats_matrix() {
    let h = common::start().await;
    h.service_with_surface(SVC).await;
    h.service_with_surface(OTHER).await;
    let mut owner = h.kernel_session().await;

    let mut failures = Vec::new();
    for &(case, expected) in MATRIX {
        for ((who, caller), want) in common::matrix_callers().into_iter().zip(expected) {
            let t = targets(&mut owner).await;
            let mut session = h.session(caller).await;
            let got = run(&mut session, case, &t).await;
            if got != want {
                failures.push(format!("{case:?} as {who}: want {want:?}, got {got:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test]
async fn service_lists_only_own_chats() {
    let h = common::start().await;
    let mut svc = h.service_with_surface(SVC).await;
    let mut other = h.service_with_surface(OTHER).await;
    let mut owner = h.kernel_session().await;
    targets(&mut owner).await;
    for (session, own) in [(&mut svc, SVC), (&mut other, OTHER)] {
        session
            .chats()
            .create()
            .call(create_args(own))
            .await
            .expect("own chat");
    }

    for (session, own) in [(&mut svc, SVC), (&mut other, OTHER)] {
        for deleted in [false, true] {
            let list::Ok { found } = session
                .chats()
                .list()
                .call(list::Args {
                    deleted,
                    ..Default::default()
                })
                .await
                .expect("list");
            assert!(
                found.iter().all(|c| c.surface_id.as_str() == own),
                "{own} sees {found:?}"
            );
            if own == OTHER && deleted {
                assert!(found.is_empty());
            } else {
                assert!(!found.is_empty(), "{own} deleted={deleted}");
            }
        }
    }
}

async fn aliased(session: &mut TestSession, surface: &str, external_id: &str) -> ChatId {
    session
        .chats()
        .create()
        .call(create::Args {
            external_id: Some(String::from(external_id)),
            ..create_args(surface)
        })
        .await
        .expect("aliased chat")
        .chat
        .id
}

/// Чужой объект в явном фильтре `chats.list` — `forbidden`, не пустая
/// страница; несуществующий — тот же отказ, чтобы не служить оракулом.
#[tokio::test]
async fn service_list_filter_must_be_own() {
    let h = common::start().await;
    let mut svc = h.service_with_surface(SVC).await;
    let mut other = h.service_with_surface(OTHER).await;
    let mut owner = h.client_session().await;
    let foreign_folder = owner
        .folders()
        .create()
        .call(folders::create::Args {
            surface: SurfaceId::from(OTHER),
            name: String::from("work"),
        })
        .await
        .expect("foreign folder")
        .folder
        .id;
    let own_folder = owner
        .folders()
        .create()
        .call(folders::create::Args {
            surface: SurfaceId::from(SVC),
            name: String::from("work"),
        })
        .await
        .expect("own folder")
        .folder
        .id;
    other
        .chats()
        .create()
        .call(create_args(OTHER))
        .await
        .expect("foreign chat");
    svc.chats()
        .create()
        .call(create::Args {
            folder: Some(own_folder),
            ..create_args(SVC)
        })
        .await
        .expect("own chat");

    let by_surface = |surface: &str| list::Args {
        surface: Some(SurfaceId::from(surface)),
        ..Default::default()
    };
    let by_folder = |folder| list::Args {
        folder: Some(folder),
        ..Default::default()
    };
    let cases = [
        ("own surface", by_surface(SVC), K),
        ("own folder", by_folder(own_folder), K),
        ("foreign surface", by_surface(OTHER), F),
        ("local", by_surface("local"), F),
        ("system", by_surface("system"), F),
        ("unknown surface", by_surface("nowhere"), F),
        ("foreign folder", by_folder(foreign_folder), F),
        ("unknown folder", by_folder(FolderId(9_999)), F),
    ];
    for (what, args, want) in cases {
        let res = svc.chats().list().call(args).await;
        assert_eq!(outcome(&res), want, "{what}: {res:?}");
        if let Ok(ok) = res {
            assert!(!ok.found.is_empty(), "{what}");
            assert!(
                ok.found.iter().all(|c| c.surface_id.as_str() == SVC),
                "{what}"
            );
        }
    }

    let res = owner.chats().list().call(by_surface("nowhere")).await;
    assert_eq!(outcome(&res), Outcome::NotFound, "owner: {res:?}");
    let res = owner.chats().list().call(by_surface(OTHER)).await;
    assert_eq!(outcome(&res), K, "owner sees every surface: {res:?}");
}

/// Якорь страницы решается под видимостью caller'а: чужой чат как якорь
/// неотличим от исчезнувшего и не выдаёт своё время активности.
#[tokio::test]
async fn service_page_anchor_must_be_visible() {
    let h = common::start().await;
    let mut svc = h.service_with_surface(SVC).await;
    let mut other = h.service_with_surface(OTHER).await;
    let mut own = Vec::new();
    for _ in 0..3 {
        own.push(
            svc.chats()
                .create()
                .call(create_args(SVC))
                .await
                .expect("own chat")
                .chat
                .id,
        );
    }
    let foreign = other
        .chats()
        .create()
        .call(create_args(OTHER))
        .await
        .expect("foreign chat")
        .chat
        .id;

    let after = |after| list::Args {
        after: Some(after),
        ..Default::default()
    };
    let page = svc
        .chats()
        .list()
        .call(after(own[2]))
        .await
        .expect("own anchor pages on");
    assert_eq!(
        page.found.iter().map(|c| c.id).collect::<Vec<_>>(),
        [own[1], own[0]]
    );

    let foreign_res = svc.chats().list().call(after(foreign)).await;
    let missing_res = svc.chats().list().call(after(ChatId(9_999))).await;
    assert_eq!(outcome(&foreign_res), Outcome::Conflict, "{foreign_res:?}");
    assert_eq!(outcome(&missing_res), Outcome::Conflict, "{missing_res:?}");

    let page = h
        .client_session()
        .await
        .chats()
        .list()
        .call(after(foreign))
        .await
        .expect("the owner may anchor on any chat");
    assert!(page.found.iter().all(|c| c.id != foreign));
}

/// `chats.get` по alias: `Service` сначала доказывает свой surface — иначе
/// ответ выдавал бы, есть ли у хозяина чат с этим внешним id.
#[tokio::test]
async fn service_cannot_probe_aliases() {
    let h = common::start().await;
    let mut svc = h.service_with_surface(SVC).await;
    let mut other = h.service_with_surface(OTHER).await;
    let mut owner = h.client_session().await;
    let mine = aliased(&mut svc, SVC, "u1").await;
    aliased(&mut other, OTHER, "u1").await;
    aliased(&mut owner, "local", "u1").await;

    let alias = |surface: &str, external_id: &str| get::Args {
        chat: Selector::Alias {
            surface: SurfaceId::from(surface),
            external_id: String::from(external_id),
        },
    };
    let ok = svc
        .chats()
        .get()
        .call(alias(SVC, "u1"))
        .await
        .expect("own alias");
    assert_eq!(ok.chat.id, mine);
    let res = svc.chats().get().call(alias(SVC, "u2")).await;
    assert_eq!(
        outcome(&res),
        Outcome::Spec,
        "own surface, no chat: {res:?}"
    );

    let mut refusals = Vec::new();
    for (surface, external_id) in [
        (OTHER, "u1"),
        (OTHER, "u2"),
        ("local", "u1"),
        ("local", "u2"),
        ("nowhere", "u1"),
    ] {
        let res = svc.chats().get().call(alias(surface, external_id)).await;
        assert_eq!(
            outcome(&res),
            F,
            "{surface}/{external_id} exists or not — same answer: {res:?}"
        );
        refusals.push(format!("{res:?}").replace(surface, "_"));
    }
    refusals.dedup();
    assert_eq!(refusals.len(), 1, "one refusal text: {refusals:?}");

    for surface in [OTHER, "nowhere"] {
        let res = owner.chats().get().call(alias(surface, "u2")).await;
        assert_eq!(outcome(&res), Outcome::Spec, "owner, {surface}: {res:?}");
    }
}

/// `chats.create` на чужом и несуществующем surface — один отказ сервису.
#[tokio::test]
async fn service_cannot_probe_surfaces_by_create() {
    let h = common::start().await;
    let mut svc = h.service_with_surface(SVC).await;
    h.service_with_surface(OTHER).await;
    for surface in [OTHER, "nowhere"] {
        let res = svc.chats().create().call(create_args(surface)).await;
        assert_eq!(outcome(&res), F, "{surface}: {res:?}");
    }
    let res = h
        .client_session()
        .await
        .chats()
        .create()
        .call(create_args("nowhere"))
        .await;
    assert_eq!(outcome(&res), Outcome::NotFound, "owner: {res:?}");
}
