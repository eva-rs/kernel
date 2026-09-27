//! Отказ SQLite доезжает ровно `Aux::Db` (`11 §2`, `§7`): край логирует его
//! иначе, чем `Internal`, поэтому хендлер не вправе их смешивать.

mod common;

use std::sync::atomic::Ordering;

use common::{Harness, Outcome, outcome};
use eva::{bytes::Bytes, time::Clock as _};
use eva_core::{
    CallStep as _,
    service::{blobs::Blobs as _, chats::Chats as _, stats::Stats as _},
};
use eva_protocol::{
    errors::Aux,
    requests::{blobs, chats, stats},
    types::{BlobId, ChatId, SurfaceId, blob, stats::Scope},
};

async fn chat(h: &Harness) -> ChatId {
    h.client_session()
        .await
        .chats()
        .create()
        .call(chats::create::Args {
            surface: SurfaceId::from("local"),
            folder: None,
            external_id: None,
            external_kind: None,
            kind: None,
            title: None,
            project: None,
            workdir: None,
            workdir_on: None,
        })
        .await
        .expect("chat")
        .chat
        .id
}

#[tokio::test]
async fn blobs_answer_aux_db() {
    let h = common::start().await;
    let chat = chat(&h).await;
    let mut client = h.client_session().await;
    let blob = client
        .blobs()
        .put()
        .call(blobs::put::Args {
            meta: blobs::put::Meta {
                chat,
                name: String::from("a.txt"),
                media_type: String::from("text/plain"),
                kind: blob::Kind::File,
                size: 3,
                hash: None,
            },
            body: Box::pin(futures::stream::iter([Ok::<_, Aux>(Bytes::from_static(
                b"abc",
            ))])),
        })
        .await
        .expect("put before the failure")
        .id;

    h.db_fail.store(true, Ordering::Relaxed);
    let mut client = h.client_session().await;
    let res = client
        .blobs()
        .stat()
        .call(blobs::stat::Args { id: blob })
        .await;
    assert_eq!(outcome(&res), Outcome::Db, "stat: {res:?}");
    let res = client
        .blobs()
        .stat()
        .call(blobs::stat::Args {
            id: BlobId::from("missing"),
        })
        .await;
    assert_eq!(
        outcome(&res),
        Outcome::Db,
        "stat of a missing blob: {res:?}"
    );
    let res = client
        .blobs()
        .put()
        .call(blobs::put::Args {
            meta: blobs::put::Meta {
                chat,
                name: String::from("b.txt"),
                media_type: String::from("text/plain"),
                kind: blob::Kind::File,
                size: 3,
                hash: None,
            },
            body: Box::pin(futures::stream::iter([Ok::<_, Aux>(Bytes::from_static(
                b"xyz",
            ))])),
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Db, "put: {res:?}");
}

#[tokio::test]
async fn stats_answer_aux_db() {
    let h = common::start().await;
    let chat = chat(&h).await;
    let day = h
        .clock
        .get()
        .date()
        .expect("the test clock is inside the calendar");

    h.db_fail.store(true, Ordering::Relaxed);
    let mut client = h.client_session().await;
    let res = client
        .stats()
        .spend()
        .call(stats::spend::Args {
            scope: Scope::Global,
            target: None,
            from: day,
            to: day,
            group_by: Default::default(),
            view: None,
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Db, "spend: {res:?}");
    let res = client
        .stats()
        .chat_spend()
        .call(stats::chat_spend::Args { chat })
        .await;
    assert_eq!(outcome(&res), Outcome::Db, "chat_spend: {res:?}");
}

/// До принятия токена пир чужой: текст отказа БД ему не уходит.
#[tokio::test]
async fn a_database_failure_before_the_token_is_accepted_stays_in_the_log() {
    use eva_protocol::wire::{
        Capabilities, ErrorCode,
        handshake::{Hello, ProtoVersion},
    };
    use eva_rpc::{ClientOptions, ConnectError, NoHandler, RpcClient};

    let h = common::start_with(common::with_module_tokens).await;
    let (_listening, url) = h.listen().await;
    h.db_fail.store(true, Ordering::Relaxed);

    let hello = Hello {
        version: String::from("1.0.0"),
        proto_version: ProtoVersion::CURRENT,
        token: "not-a-config-token".into(),
        capabilities: Capabilities::default(),
        resume: None,
    };
    let res = RpcClient::connect_with(&url, hello, ClientOptions::default(), NoHandler).await;
    let Err(ConnectError::Rejected(error)) = res else {
        panic!("expected a rejected hello, got {res:?}");
    };
    assert_eq!(error.code, ErrorCode::Internal, "{error:?}");
    assert!(
        !error.message.contains("injected"),
        "database details leak to an unauthenticated peer: {error:?}"
    );
}
