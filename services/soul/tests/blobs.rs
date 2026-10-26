//! `blobs.*` (`07 §6`, `06 §5`): приём с проверкой по ходу, `Service` —
//! только blob'ы чатов своих surface; отменённый приём не оставляет файла,
//! живые blob'ы владельца — до `work.blob_quota`.

mod common;

use std::time::Duration;

use common::{Harness, OTHER, Outcome, SVC, TestSession, matrix_callers, outcome};
use eva::bytesize::ByteSize;

use eva_protocol::{
    errors::{Aux, Generic},
    requests::{
        Blobs::{get, put, stat},
        ByteStream,
    },
    types::{BlobId, ChatId, blob::Kind},
};
use futures::StreamExt as _;

use eva_soul::{
    db::{Db as _, DbResult, blobs::BLOB_FLOOR},
    handling::blobs::{STAGING, sweep},
};

const DATA: &[u8] = b"the quick brown fox jumps over the lazy dog";

type PutResult = Result<put::Ok, Generic<put::Err>>;

async fn chat(h: &Harness, surface: &str) -> ChatId {
    let surface = surface.to_owned();
    h.soul
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

fn args(chat: ChatId, name: &str, size: usize) -> put::Meta {
    put::Meta {
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
        .put(
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
    let mut items = session.blobs().get(id.clone(), offset).await?.0;
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
    std::fs::read_dir(h.dir.path().join("blobs")).map_or(0, |dir| {
        dir.filter(|e| e.as_ref().map_or(true, |e| e.file_name() != STAGING))
            .count()
    })
}

/// Недописанные приёмы в `blob_dir/.staging`.
fn staged(h: &Harness) -> usize {
    std::fs::read_dir(h.dir.path().join("blobs").join(STAGING)).map_or(0, |dir| dir.count())
}

/// Условие наступает за `within`: фоновая уборка отменённого приёма.
async fn eventually(within: Duration, mut check: impl FnMut() -> bool) -> bool {
    let until = std::time::Instant::now() + within;
    while std::time::Instant::now() < until {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    check()
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
        .stat(id.clone())
        .await
        .map(|ok| ok.0)
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
        .put(
            put::Meta {
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
        .put(
            put::Meta {
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
        .put(
            put::Meta {
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
        .put(args(chat, "short", 10), body(&[b"abc"]))
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(put::Err::SizeMismatch(e))) if e.declared == 10 && e.actual == 3),
        "{res:?}"
    );

    let res = client
        .blobs()
        .put(args(chat, "long", 4), body(&[b"abc", b"def", b"ghi"]))
        .await;
    assert!(
        matches!(&res, Err(Generic::Spec(put::Err::SizeMismatch(e))) if e.declared == 4 && e.actual == 6),
        "stops at the first chunk past the declared size: {res:?}"
    );

    let res = client
        .blobs()
        .put(args(chat, "huge", 65), body(&[&[0; 65]]))
        .await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Quota(_)))), "{res:?}");

    let broken: ByteStream = Box::pin(futures::stream::iter([
        Ok(eva::bytes::Bytes::from_static(b"abc")),
        Err(Aux::Unavailable(String::from("socket closed"))),
    ]));
    let res = client.blobs().put(args(chat, "cut", 6), broken).await;
    assert!(
        matches!(res, Err(Generic::Aux(Aux::Unavailable(_)))),
        "{res:?}"
    );

    assert_eq!(stored(&h), 0, "rejected uploads leave nothing behind");
    assert_eq!(
        staged(&h),
        0,
        "their staging files are gone before the reply"
    );
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
        let res = client.blobs().put(args(chat, name, 1), body(&[b"x"])).await;
        assert!(
            matches!(&res, Err(Generic::Spec(put::Err::BadName(e))) if e.name == name),
            "{name:?}: {res:?}"
        );
    }
    let res = client
        .blobs()
        .put(
            put::Meta {
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
    let res = client.blobs().stat(unknown.clone()).await.map(|ok| ok.0);
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
        .put(args(ChatId(999), "x", 1), body(&[b"x"]))
        .await;
    assert!(
        matches!(&res, Err(Generic::Aux(Aux::NotFound(what))) if what.contains("c999")),
        "chat is a second-order reference: {res:?}"
    );

    let chat = chat(&h, "local").await;
    let put::Ok { id, .. } = upload(&mut client, chat).await;
    h.clock.advance(Duration::from_secs(30 * 24 * 60 * 60));
    let mut client = h.client_session().await;
    let res = client.blobs().stat(id).await.map(|ok| ok.0);
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
    let mut soul = h.soul_session().await;
    let blob_own = upload(&mut soul, own).await.id;
    let blob_local = upload(&mut soul, local).await.id;
    // Чат blob'а удалён: `chat_id` обнулился каскадом.
    let orphan = upload(&mut soul, local).await.id;
    let id = orphan.clone();
    h.soul
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
    // soul, client, service-own, service-foreign
    let cases: [(&str, ChatId, BlobId, [Outcome; 4]); 3] = [
        ("own surface", own, blob_own, [K, K, K, F]),
        ("local", local, blob_local, [K, K, F, F]),
        ("no chat", local, orphan, [K, K, F, F]),
    ];

    for (case, chat, blob, expect) in cases {
        for ((who, caller), expect) in matrix_callers().into_iter().zip(expect) {
            let mut session = h.session(caller).await;
            let res = session.blobs().stat(blob.clone()).await.map(|ok| ok.0);
            assert_eq!(outcome(&res), expect, "{case}: {who} stat");
            let res = download(&mut session, &blob, 0).await.map(|_| ());
            assert_eq!(outcome(&res), expect, "{case}: {who} get");
            if case != "no chat" {
                let res = session
                    .blobs()
                    .put(args(chat, "p.txt", 1), body(&[b"p"]))
                    .await;
                assert_eq!(outcome(&res), expect, "{case}: {who} put");
            }
        }
    }
}

#[tokio::test]
async fn cancelled_upload_leaves_nothing() {
    let h = common::start().await;
    let chat = chat(&h, "local").await;
    let mut client = h.client_session().await;
    let stalled: ByteStream = Box::pin(
        futures::stream::iter([Ok(eva::bytes::Bytes::from_static(b"partial"))])
            .chain(futures::stream::pending()),
    );
    let upload = tokio::spawn(async move {
        client
            .blobs()
            .put(args(chat, "cut.bin", 64), stalled)
            .await
            .is_ok()
    });

    assert!(
        eventually(Duration::from_secs(5), || staged(&h) == 1).await,
        "the upload is being received into staging"
    );
    upload.abort();
    assert!(upload.await.is_err(), "the call was cancelled");
    assert!(
        eventually(Duration::from_secs(5), || staged(&h) == 0).await,
        "the partial file is removed"
    );
    assert_eq!(stored(&h), 0);
    let rows: i64 = h
        .query(|c| Ok(c.query_row("SELECT count(*) FROM blobs", [], |r| r.get(0))?))
        .await;
    assert_eq!(rows, 0);
}

#[tokio::test]
async fn startup_sweep_removes_stale_uploads() {
    let h = common::start().await;
    let blob_dir = h.dir.path().join("blobs");
    let staging = blob_dir.join(STAGING);
    std::fs::create_dir_all(staging.join("odd-dir")).expect("staging dir");
    std::fs::write(staging.join("0190aaaa-stale"), b"half").expect("stale upload");
    let kept = blob_dir.join("0190aaaa-kept");
    std::fs::create_dir_all(&kept).expect("stored blob dir");
    std::fs::write(kept.join("f.txt"), b"whole").expect("stored blob");

    assert_eq!(sweep(&blob_dir).await.expect("sweep"), 2);
    assert_eq!(staged(&h), 0);
    assert!(kept.join("f.txt").exists(), "stored blobs stay");
    assert_eq!(
        sweep(&h.dir.path().join("nowhere")).await.expect("no dir"),
        0
    );
}

#[tokio::test]
async fn soul_start_sweeps_stale_uploads() {
    let common::Harness { soul, dir, .. } = common::start().await;
    drop(soul);
    let blob_dir = dir.path().join("blobs");
    let staging = blob_dir.join(STAGING);
    std::fs::create_dir_all(&staging).expect("staging dir");
    std::fs::write(staging.join("0190aaaa-stale"), b"half").expect("stale upload");

    let cfg = common::config(dir.path());
    let h = common::start_in(dir, cfg).await;
    assert_eq!(staged(&h), 0, "a restart drops half-received uploads");
}

#[tokio::test]
async fn live_blobs_of_an_owner_share_a_quota() {
    // Два blob'а по `BLOB_FLOOR` влезают, третий — нет.
    let mut h =
        common::start_with(|cfg| cfg.work.blob_quota = ByteSize::b(3 * BLOB_FLOOR - 1)).await;
    let mut svc = h.service_with_surface(SVC).await;
    let mut other = h.service_with_surface(OTHER).await;
    let own = chat(&h, SVC).await;
    let foreign = chat(&h, OTHER).await;

    upload(&mut svc, own).await;
    upload(&mut svc, own).await;
    let res = svc
        .blobs()
        .put(args(own, "fox.txt", DATA.len()), body(&[DATA]))
        .await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Quota(_)))), "{res:?}");
    let mut soul = h.soul_session().await;
    let res = soul
        .blobs()
        .put(args(own, "fox.txt", DATA.len()), body(&[DATA]))
        .await;
    assert!(
        matches!(res, Err(Generic::Aux(Aux::Quota(_)))),
        "the chat's owner pays, not the caller: {res:?}"
    );
    upload(&mut other, foreign).await;

    h.clock.advance(Duration::from_secs(30 * 24 * 60 * 60));
    let mut svc = h.session(common::service(SVC)).await;
    upload(&mut svc, own).await;
}

#[tokio::test]
async fn concurrent_uploads_cannot_overshoot_the_quota() {
    let h = common::start_with(|cfg| cfg.work.blob_quota = ByteSize::b(2 * BLOB_FLOOR - 1)).await;
    let chat = chat(&h, "local").await;
    let mut a = h.client_session().await;
    let mut b = h.client_session().await;
    async fn put_60(session: &mut TestSession, chat: ChatId, name: &str) -> PutResult {
        session
            .blobs()
            .put(args(chat, name, 60), body(&[&[1; 60]]))
            .await
    }

    let (ra, rb) = tokio::join!(put_60(&mut a, chat, "a"), put_60(&mut b, chat, "b"));
    assert_eq!(
        usize::from(ra.is_ok()) + usize::from(rb.is_ok()),
        1,
        "{ra:?} {rb:?}"
    );
    assert!(
        [&ra, &rb]
            .iter()
            .all(|r| r.is_ok() || matches!(r, Err(Generic::Aux(Aux::Quota(_))))),
        "{ra:?} {rb:?}"
    );
    assert_eq!(stored(&h), 1, "the refused upload left no directory");
    assert_eq!(staged(&h), 0);
}

/// Пустой blob — тоже каталог, файл и строка: байтовая квота видит его по `BLOB_FLOOR`.
#[tokio::test]
async fn empty_blobs_are_charged_a_floor() {
    let h = common::start_with(|cfg| cfg.work.blob_quota = ByteSize::kib(64)).await;
    let mut svc = h.service_with_surface(SVC).await;
    let own = chat(&h, SVC).await;

    let fit = 64 * 1024 / BLOB_FLOOR;
    for n in 0..fit {
        svc.blobs()
            .put(args(own, &format!("empty-{n}"), 0), body(&[]))
            .await
            .unwrap_or_else(|e| panic!("empty blob {n}: {e:?}"));
    }
    let res = svc
        .blobs()
        .put(args(own, "one-too-many", 0), body(&[]))
        .await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Quota(_)))), "{res:?}");
    assert_eq!(stored(&h), usize::try_from(fit).expect("fits"));
}
