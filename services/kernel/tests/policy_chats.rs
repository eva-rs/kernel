//! Матрица `Policy` (`11 §8`) для `chats.*`: surface чата решает, `Service`
//! видит только свои; жизненный цикл — только хозяину.

mod common;

use common::{OTHER, Outcome, SVC, TestSession, outcome};
use eva_core::{CallStep as _, service::chats::Chats as _};
use eva_protocol::{
    requests::{
        Patch,
        chats::{
            compact, create, delete, get, history_read, list, r#move, notify, restore, search,
            update,
        },
    },
    types::{ChatId, SurfaceId, folder},
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
    (Case::Search, [U, U, F, F]),
    (Case::Compact, [U, U, F, F]),
    (Case::Notify, [U, F, F, F]),
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
