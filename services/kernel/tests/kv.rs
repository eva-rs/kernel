//! `kv.*` (`06 §5`, I12): KV модуля, last-write-wins, скоуп — имя модуля.

mod common;

use std::sync::atomic::Ordering;

use common::{OTHER, Outcome, SVC, TestSession, matrix_callers, outcome, service};
use eva_core::{CallStep as _, service::kv::Kv as _};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::{
        Limit,
        kv::{del, get, list, set},
    },
};
use serde_json::{Value, json};

async fn put(session: &mut TestSession, key: &str, value: Value) {
    session
        .kv()
        .set()
        .call(set::Args {
            key: key.to_owned(),
            value,
        })
        .await
        .expect("kv.set");
}

async fn read(session: &mut TestSession, key: &str) -> Option<Value> {
    session
        .kv()
        .get()
        .call(get::Args {
            key: key.to_owned(),
        })
        .await
        .expect("kv.get")
        .value
}

async fn keys(session: &mut TestSession, args: list::Args) -> Vec<String> {
    session.kv().list().call(args).await.expect("kv.list").keys
}

#[tokio::test]
async fn set_get_del_last_write_wins() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;

    assert_eq!(read(&mut svc, "offset").await, None);
    let doc = json!({"n": 1, "tags": ["a", "b"], "nested": {"f": 1.5, "t": true, "z": null}});
    put(&mut svc, "offset", doc.clone()).await;
    assert_eq!(read(&mut svc, "offset").await, Some(doc));

    put(&mut svc, "offset", json!(42)).await;
    assert_eq!(read(&mut svc, "offset").await, Some(json!(42)));

    svc.kv()
        .del()
        .call(del::Args {
            key: String::from("offset"),
        })
        .await
        .expect("del");
    assert_eq!(read(&mut svc, "offset").await, None);

    svc.kv()
        .del()
        .call(del::Args {
            key: String::from("offset"),
        })
        .await
        .expect("deleting a missing key is not an error");
}

#[tokio::test]
async fn modules_do_not_see_each_other() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;
    let mut other = h.session(service(OTHER)).await;
    let mut client = h.client_session().await;

    put(&mut svc, "k", json!("svc")).await;
    put(&mut client, "k", json!("client")).await;

    assert_eq!(read(&mut other, "k").await, None);
    assert_eq!(read(&mut svc, "k").await, Some(json!("svc")));
    assert_eq!(read(&mut client, "k").await, Some(json!("client")));
    assert!(keys(&mut other, list::Args::default()).await.is_empty());

    other
        .kv()
        .del()
        .call(del::Args {
            key: String::from("k"),
        })
        .await
        .expect("del");
    assert_eq!(read(&mut svc, "k").await, Some(json!("svc")));
}

#[tokio::test]
async fn list_by_prefix_with_keyset_pages() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;
    for key in [
        "chat:3", "chat:1", "chat:2", "chat%x", "chat_y", "user:1", "chat:4",
    ] {
        put(&mut svc, key, json!(key)).await;
    }

    let limit = Limit::new(2).expect("limit");
    let page = |after: Option<&str>| list::Args {
        prefix: String::from("chat:"),
        limit,
        after: after.map(str::to_owned),
    };
    assert_eq!(keys(&mut svc, page(None)).await, ["chat:1", "chat:2"]);
    assert_eq!(
        keys(&mut svc, page(Some("chat:2"))).await,
        ["chat:3", "chat:4"]
    );
    let last = keys(&mut svc, page(Some("chat:4"))).await;
    assert!(last.is_empty(), "end of the prefix: {last:?}");

    let all = keys(&mut svc, list::Args::default()).await;
    assert_eq!(
        all,
        [
            "chat%x", "chat:1", "chat:2", "chat:3", "chat:4", "chat_y", "user:1"
        ],
        "ascending by key"
    );

    let wild = keys(
        &mut svc,
        list::Args {
            prefix: String::from("chat%"),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(wild, ["chat%x"], "% is not a wildcard");
    let wild = keys(
        &mut svc,
        list::Args {
            prefix: String::from("chat_"),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(wild, ["chat_y"], "_ is not a wildcard");
}

#[tokio::test]
async fn key_is_validated() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;

    for key in [String::new(), "k".repeat(1025)] {
        let res = svc.kv().get().call(get::Args { key: key.clone() }).await;
        assert_eq!(outcome(&res), Outcome::Invalid, "get {}", key.len());
        let res = svc
            .kv()
            .set()
            .call(set::Args {
                key: key.clone(),
                value: json!(1),
            })
            .await;
        assert_eq!(outcome(&res), Outcome::Invalid, "set {}", key.len());
        let res = svc.kv().del().call(del::Args { key }).await;
        assert_eq!(outcome(&res), Outcome::Invalid, "del");
    }
}

#[tokio::test]
async fn policy_matrix() {
    let h = common::start().await;

    for (who, caller) in matrix_callers() {
        let expect = if caller.is_kernel() {
            Outcome::Forbidden
        } else {
            Outcome::Ok
        };
        let mut session = h.session(caller).await;
        let key = String::from("k");

        let res = session
            .kv()
            .set()
            .call(set::Args {
                key: key.clone(),
                value: json!(1),
            })
            .await;
        assert_eq!(outcome(&res), expect, "{who} set");
        let res = session
            .kv()
            .get()
            .call(get::Args { key: key.clone() })
            .await;
        assert_eq!(outcome(&res), expect, "{who} get");
        let res = session.kv().list().call(list::Args::default()).await;
        assert_eq!(outcome(&res), expect, "{who} list");
        let res = session.kv().del().call(del::Args { key }).await;
        assert_eq!(outcome(&res), expect, "{who} del");
    }
}

#[tokio::test]
async fn database_failure_is_aux_db() {
    let h = common::start().await;
    let mut svc = h.session(service(SVC)).await;
    h.db_fail.store(true, Ordering::Relaxed);

    let res = svc
        .kv()
        .set()
        .call(set::Args {
            key: String::from("k"),
            value: json!(1),
        })
        .await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Db(_)))), "{res:?}");
    let res = svc
        .kv()
        .get()
        .call(get::Args {
            key: String::from("k"),
        })
        .await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Db(_)))), "{res:?}");
}
