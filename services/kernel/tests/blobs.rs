//! `blobs.*` (`07 §6`, `06 §5`): приём с проверкой по ходу, `Service` —
//! только blob'ы чатов своих surface.

mod common;

use std::time::Duration;

use common::{Harness, OTHER, Outcome, SVC, TestSession, matrix_callers, outcome};
use eva_core::{
    ByteStream, CallStep as _, StreamStep as _, UploadStep as _, service::blobs::Blobs as _,
};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::blobs::{get, put, stat},
    types::{BlobId, ChatId, blob::Kind},
};
use futures::StreamExt as _;

use eva_kernel::db::{Db as _, DbResult};

const DATA: &[u8] = b"the quick brown fox jumps over the lazy dog";

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

fn body(chunks: &[&[u8]]) -> ByteStream {
    let chunks: Vec<Result<eva::bytes::Bytes, Aux>> = chunks
        .iter()
        .map(|c| Ok(eva::bytes::Bytes::copy_from_slice(c)))
        .collect();
    Box::pin(futures::stream::iter(chunks))
}

fn args(chat: ChatId, name: &str, size: usize) -> put::Args {
    put::Args {
        chat,
        name: name.to_owned(),
        media_type: String::from("text/plain"),
        kind: Kind::File,
        size: size as u64,
        hash: None,
    }
}

fn hex(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

async fn upload(session: &mut TestSession, chat: ChatId) -> put::Ok {
    session
        .blobs()
        .put()
        .call(
            args(chat, "fox.txt", DATA.len()),
            body(&[&DATA[..10], &DATA[10..]]),
        )
        .await
        .expect("blobs.put")
}

async fn download(
    session: &mut TestSession,
    id: &BlobId,
    offset: u64,
) -> Result<(get::Item, Vec<u8>, usize), Generic<get::Err>> {
    let mut items = session
        .blobs()
        .get()
        .open(get::Args {
            id: id.clone(),
            offset,
        })
        .await?;
    let meta = items.next().await.expect("meta item")?;
    let (mut data, mut chunks) = (Vec::new(), 0);
    while let Some(item) = items.next().await {
        match item? {
            get::Item::Chunk(bytes) => {
                data.extend_from_slice(&bytes);
                chunks += 1;
            }
            other => panic!("unexpected item {other:?}"),
        }
    }
    Ok((meta, data, chunks))
}

/// Каталоги blob'ов на диске: отвергнутые не оставляют следа.
fn stored(h: &Harness) -> usize {
    std::fs::read_dir(h.dir.path().join("blobs")).map_or(0, |dir| dir.count())
}

#[tokio::test]
async fn put_get_stat_roundtrip() {
    let h = common::start_with(|cfg| {
        cfg.modules.transport.blob_chunk = eva::bytesize::ByteSize::b(16);
    })
    .await;
    let mut svc = h.service_with_surface(SVC).await;
    let chat = chat(&h, SVC).await;

    let put::Ok { id, size, hash } = upload(&mut svc, chat).await;
    assert_eq!(size, DATA.len() as u64);
    assert_eq!(hash, hex(DATA));
    let on_disk = h.dir.path().join("blobs").join(id.as_str()).join("fox.txt");
    assert_eq!(std::fs::read(on_disk).expect("stored file"), DATA);

    let (meta, data, chunks) = download(&mut svc, &id, 0).await.expect("get");
    assert_eq!(
        meta,
        get::Item::Meta {
            name: String::from("fox.txt"),
            media_type: String::from("text/plain"),
            size: DATA.len() as u64,
        }
    );
    assert_eq!(data, DATA);
    assert_eq!(
        chunks,
        DATA.len().div_ceil(16),
        "chunks follow modules.blob_chunk"
    );

    let (_, tail, _) = download(&mut svc, &id, 40).await.expect("resume");
    assert_eq!(tail, &DATA[40..]);
    let (_, empty, chunks) = download(&mut svc, &id, DATA.len() as u64)
        .await
        .expect("offset at the end");
    assert!(empty.is_empty() && chunks == 0);
    let res = download(&mut svc, &id, DATA.len() as u64 + 1).await;
    assert_eq!(outcome(&res.map(|_| ())), Outcome::Invalid);

    let meta = svc
        .blobs()
        .stat()
        .call(stat::Args { id: id.clone() })
        .await
        .expect("stat");
    assert_eq!(meta.name, "fox.txt");
    assert_eq!(meta.size, DATA.len() as u64);
    assert_eq!(meta.hash, hex(DATA));
    assert_eq!(meta.kind, Kind::File);
    let keep = Duration::from_secs(30 * 24 * 60 * 60).as_nanos() as u64;
    let created: i64 = {
        let id = id.clone();
        h.query(move |c| {
            Ok(c.query_row(
                "SELECT created_at FROM blobs WHERE id = ?1 AND source = 'upload'",
                [id.as_str()],
                |r| r.get(0),
            )?)
        })
        .await
    };
    assert_eq!(meta.expires_at.as_nanos().get(), created as u64 + keep);
}

#[tokio::test]
async fn declared_hash_is_checked() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    let chat = chat(&h, "local").await;

    let ok = client
        .blobs()
        .put()
        .call(
            put::Args {
                hash: Some(hex(DATA)),
                ..args(chat, "fox.txt", DATA.len())
            },
            body(&[DATA]),
        )
        .await
        .expect("matching hash");
    assert_eq!(ok.hash, hex(DATA));

    let wrong = hex(b"something else");
    let res = client
        .blobs()
        .put()
        .call(
            put::Args {
                hash: Some(wrong.clone()),
                ..args(chat, "fox.txt", DATA.len())
            },
            body(&[DATA]),
        )
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(put::Err::HashMismatch(e))) if e.declared == wrong && e.actual == hex(DATA)),
        "{res:?}"
    );

    let res = client
        .blobs()
        .put()
        .call(
            put::Args {
                hash: Some(String::from("not-hex")),
                ..args(chat, "fox.txt", DATA.len())
            },
            body(&[DATA]),
        )
        .await;
    assert_eq!(outcome(&res), Outcome::Invalid);
    assert_eq!(stored(&h), 1, "only the accepted blob is on disk");
}

#[tokio::test]
async fn size_is_checked_while_receiving() {
    let h = common::start_with(|cfg| cfg.work.blob_max = eva::bytesize::ByteSize::b(64)).await;
    let mut client = h.client_session().await;
    let chat = chat(&h, "local").await;

    let res = client
        .blobs()
        .put()
        .call(args(chat, "short", 10), body(&[b"abc"]))
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(put::Err::SizeMismatch(e))) if e.declared == 10 && e.actual == 3),
        "{res:?}"
    );

    let res = client
        .blobs()
        .put()
        .call(args(chat, "long", 4), body(&[b"abc", b"def", b"ghi"]))
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(put::Err::SizeMismatch(e))) if e.declared == 4 && e.actual == 6),
        "stops at the first chunk past the declared size: {res:?}"
    );

    let res = client
        .blobs()
        .put()
        .call(args(chat, "huge", 65), body(&[&[0; 65]]))
        .await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Quota(_)))), "{res:?}");

    let broken: ByteStream = Box::pin(futures::stream::iter([
        Ok(eva::bytes::Bytes::from_static(b"abc")),
        Err(Aux::Unavailable(String::from("socket closed"))),
    ]));
    let res = client
        .blobs()
        .put()
        .call(args(chat, "cut", 6), broken)
        .await;
    assert!(
        matches!(res, Err(Generic::Aux(Aux::Unavailable(_)))),
        "{res:?}"
    );

    assert_eq!(stored(&h), 0, "rejected uploads leave nothing behind");
    let rows: i64 = h
        .query(|c| Ok(c.query_row("SELECT count(*) FROM blobs", [], |r| r.get(0))?))
        .await;
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn name_must_be_a_leaf() {
    let h = common::start().await;
    let mut client = h.client_session().await;
    let chat = chat(&h, "local").await;

    for name in [
        "",
        ".",
        "..",
        "a/b",
        "../x",
        "a\\b",
        "line\nbreak",
        &"n".repeat(256),
    ] {
        let res = client
            .blobs()
            .put()
            .call(args(chat, name, 1), body(&[b"x"]))
            .await;
        assert!(
            matches!(&res, Err(Generic::Spec(put::Err::BadName(e))) if e.name == name),
            "{name:?}: {res:?}"
        );
    }
    let res = client
        .blobs()
        .put()
        .call(
            put::Args {
                media_type: String::new(),
                ..args(chat, "ok", 1)
            },
            body(&[b"x"]),
        )
        .await;
    assert_eq!(outcome(&res), Outcome::Invalid, "{res:?}");
}

#[tokio::test]
async fn missing_and_expired_blobs() {
    let mut h = common::start().await;
    let mut client = h.client_session().await;

    let unknown = BlobId::from("0190aaaa-0000-7000-8000-000000000000");
    let res = client
        .blobs()
        .stat()
        .call(stat::Args {
            id: unknown.clone(),
        })
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(stat::Err::NotFound(e))) if e.id == unknown),
        "{res:?}"
    );
    let res = download(&mut client, &unknown, 0).await;
    assert!(
        matches!(&res, Err(Generic::Spec(get::Err::NotFound(_)))),
        "{res:?}"
    );

    let res = client
        .blobs()
        .put()
        .call(args(ChatId(999), "x", 1), body(&[b"x"]))
        .await;
    assert!(
        matches!(&res, Err(Generic::Aux(Aux::NotFound(what))) if what.contains("c999")),
        "chat is a second-order reference: {res:?}"
    );

    let chat = chat(&h, "local").await;
    let put::Ok { id, .. } = upload(&mut client, chat).await;
    h.clock.advance(Duration::from_secs(30 * 24 * 60 * 60));
    let mut client = h.client_session().await;
    let res = client.blobs().stat().call(stat::Args { id }).await;
    assert!(
        matches!(&res, Err(Generic::Spec(stat::Err::NotFound(_)))),
        "expired: {res:?}"
    );
}

#[tokio::test]
async fn policy_matrix() {
    let h = common::start().await;
    let _svc = h.service_with_surface(SVC).await;
    let _other = h.service_with_surface(OTHER).await;
    let own = chat(&h, SVC).await;
    let local = chat(&h, "local").await;
    let mut kernel = h.kernel_session().await;
    let blob_own = upload(&mut kernel, own).await.id;
    let blob_local = upload(&mut kernel, local).await.id;
    // Чат blob'а удалён: `chat_id` обнулился каскадом.
    let orphan = upload(&mut kernel, local).await.id;
    let id = orphan.clone();
    h.kernel
        .shared()
        .db()
        .write(move |tx| -> DbResult<()> {
            tx.execute(
                "UPDATE blobs SET chat_id = NULL WHERE id = ?1",
                [id.as_str()],
            )?;
            Ok(())
        })
        .await
        .expect("orphan blob");

    use Outcome::{Forbidden as F, Ok as K};
    // kernel, client, service-own, service-foreign
    let cases: [(&str, ChatId, BlobId, [Outcome; 4]); 3] = [
        ("own surface", own, blob_own, [K, K, K, F]),
        ("local", local, blob_local, [K, K, F, F]),
        ("no chat", local, orphan, [K, K, F, F]),
    ];

    for (case, chat, blob, expect) in cases {
        for ((who, caller), expect) in matrix_callers().into_iter().zip(expect) {
            let mut session = h.session(caller).await;
            let res = session
                .blobs()
                .stat()
                .call(stat::Args { id: blob.clone() })
                .await;
            assert_eq!(outcome(&res), expect, "{case}: {who} stat");
            let res = download(&mut session, &blob, 0).await.map(|_| ());
            assert_eq!(outcome(&res), expect, "{case}: {who} get");
            if case != "no chat" {
                let res = session
                    .blobs()
                    .put()
                    .call(args(chat, "p.txt", 1), body(&[b"p"]))
                    .await;
                assert_eq!(outcome(&res), expect, "{case}: {who} put");
            }
        }
    }
}
