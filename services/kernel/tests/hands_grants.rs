//! `hands_grants.*` (`07 §3.1`, `11 §3`): только хозяину.

mod common;

use std::sync::atomic::Ordering;

use common::{Harness, Outcome, SVC, matrix_callers, outcome};
use eva::time::Clock as _;
use eva_core::{CallStep as _, service::HandsGrants::Trait as _};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::hands_grants::{list, revoke},
    types::{
        ChatId, HandsGrantId, PersonId, SurfaceId,
        hands::{Grant, GrantKind},
    },
};

use eva_kernel::db::{Db as _, DbResult, Queries as _, hands_grants::NewGrant};

async fn chat(h: &Harness, surface: &str) -> ChatId {
    let surface = surface.to_owned();
    h.kernel
        .shared()
        .db()
        .write(move |tx| -> DbResult<ChatId> {
            Ok(ChatId(tx.query_row(
                "INSERT INTO chats (folder_id, kind, created_at, last_activity_at)
                 SELECT id, 'persistent', 1, 1 FROM folders WHERE surface_id = ?1 AND name = 'Unsorted'
                 RETURNING id",
                [surface],
                |r| r.get(0),
            )?))
        })
        .await
        .expect("insert chat")
}

async fn grant(
    h: &Harness,
    endpoint: &str,
    surface: &str,
    chat: Option<ChatId>,
    kind: GrantKind,
    prefix: &str,
) -> HandsGrantId {
    let grant = NewGrant {
        endpoint: endpoint.to_owned(),
        surface: SurfaceId::from(surface),
        chat,
        kind,
        prefix: prefix.to_owned(),
        issued_by: PersonId::system(),
        created_at: h.clock.get(),
    };
    h.kernel
        .shared()
        .db()
        .write(move |tx| tx.hands_grants().insert(&grant))
        .await
        .expect("insert grant")
}

fn ids(grants: &[Grant]) -> Vec<HandsGrantId> {
    grants.iter().map(|g| g.id).collect()
}

#[tokio::test]
async fn list_filters_and_orders_newest_first() {
    let h = common::start().await;
    let _svc = h.service_with_surface(SVC).await;
    let c1 = chat(&h, "local").await;
    let c2 = chat(&h, SVC).await;

    let g1 = grant(&h, "local", "local", None, GrantKind::Read, "/home").await;
    let g2 = grant(&h, "local", "local", Some(c1), GrantKind::Write, "/home/w").await;
    let g3 = grant(&h, "box", SVC, Some(c2), GrantKind::Shell, "git ").await;

    let mut client = h.client_session().await;
    let mut call = async |args: list::Args| {
        client
            .hands_grants()
            .inner_mut()
            .list()
            .call(args)
            .await
            .expect("hands_grants.list")
            .grants
    };

    let all = call(list::Args::default()).await;
    assert_eq!(ids(&all), [g3, g2, g1]);
    let first = all.last().expect("g1");
    assert_eq!(first.endpoint, "local");
    assert_eq!(first.surface.as_str(), "local");
    assert_eq!(first.chat, None);
    assert_eq!(first.kind, GrantKind::Read);
    assert_eq!(first.prefix, "/home");
    assert_eq!(first.issued_by, PersonId::system());

    let by_endpoint = call(list::Args {
        endpoint: Some(String::from("box")),
        ..Default::default()
    })
    .await;
    assert_eq!(ids(&by_endpoint), [g3]);

    let by_surface = call(list::Args {
        surface: Some(SurfaceId::from("local")),
        ..Default::default()
    })
    .await;
    assert_eq!(ids(&by_surface), [g2, g1]);

    let by_chat = call(list::Args {
        chat: Some(c1),
        ..Default::default()
    })
    .await;
    assert_eq!(ids(&by_chat), [g2]);

    let none = call(list::Args {
        endpoint: Some(String::from("box")),
        surface: Some(SurfaceId::from("local")),
        chat: None,
    })
    .await;
    assert!(none.is_empty(), "filters combine with AND");
}

#[tokio::test]
async fn revoke_deletes_the_grant() {
    let h = common::start().await;
    let g1 = grant(&h, "local", "local", None, GrantKind::Read, "/").await;
    let g2 = grant(&h, "local", "local", None, GrantKind::Shell, "ls").await;
    let mut client = h.client_session().await;

    client.hands_grants().revoke(g1).await.expect("revoke");
    let left = client
        .hands_grants()
        .inner_mut()
        .list()
        .call(list::Args::default())
        .await
        .expect("list")
        .grants;
    assert_eq!(ids(&left), [g2]);

    let res = client.hands_grants().revoke(g1).await;
    assert!(
        matches!(&res, Err(Generic::Spec(revoke::Err::NotFound(e))) if e.id == g1),
        "{res:?}"
    );
}

#[tokio::test]
async fn policy_matrix() {
    let h = common::start().await;
    let _svc = h.service_with_surface(SVC).await;

    for (who, caller) in matrix_callers() {
        let expect = if caller.is_owner() {
            Outcome::Ok
        } else {
            Outcome::Forbidden
        };
        let id = grant(&h, "local", SVC, None, GrantKind::Read, "/").await;
        let mut session = h.session(caller).await;

        let res = session
            .hands_grants()
            .inner_mut()
            .list()
            .call(list::Args {
                surface: Some(SurfaceId::from(SVC)),
                ..Default::default()
            })
            .await;
        assert_eq!(outcome(&res), expect, "{who} list of its own surface");

        let res = session.hands_grants().revoke(id).await;
        assert_eq!(outcome(&res), expect, "{who} revoke");
    }
}

#[tokio::test]
async fn database_failure_is_aux_db() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    h.db_fail.store(true, Ordering::Relaxed);

    let res = client
        .hands_grants()
        .inner_mut()
        .list()
        .call(list::Args::default())
        .await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Db(_)))), "{res:?}");
    let res = client.hands_grants().revoke(HandsGrantId(1)).await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Db(_)))), "{res:?}");
}
