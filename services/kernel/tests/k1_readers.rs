//! Пул читателей (`10 §7`): не больше `db.readers` соединений, ожидание — до
//! `busy_timeout`, отменённый запрос держит своё место до конца.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use eva_kernel::db::{Db as _, DbResult, Sqlite};

/// Запрос, который занимает соединение `hold` и отмечает пик одновременных.
async fn busy_read(
    db: &Sqlite,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    hold: Duration,
) -> DbResult<()> {
    let reader = db.reader().await?;
    reader
        .run(move |_| {
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(hold);
            active.fetch_sub(1, Ordering::SeqCst);
            DbResult::Ok(())
        })
        .await
}

async fn open(dir: &tempfile::TempDir, readers: usize, busy: Duration) -> Sqlite {
    Sqlite::open(&dir.path().join("eva.sqlite"), busy, readers)
        .await
        .expect("open database")
}

#[tokio::test]
async fn concurrent_reads_never_exceed_the_pool() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = open(&dir, 2, Duration::from_secs(5)).await;
    let (active, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));

    // Живой `Reader` без запроса соединения не держит.
    let _idle = db.reader().await.expect("reader");
    let reads = (0..6).map(|_| {
        busy_read(
            &db,
            active.clone(),
            peak.clone(),
            Duration::from_millis(100),
        )
    });
    for res in futures::future::join_all(reads).await {
        res.expect("every read gets a connection in turn");
    }
    assert_eq!(peak.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn waiting_for_a_reader_is_bounded() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = open(&dir, 1, Duration::from_millis(100)).await;
    let (active, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));

    let (held, waited) = tokio::join!(
        busy_read(
            &db,
            active.clone(),
            peak.clone(),
            Duration::from_millis(600)
        ),
        async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            busy_read(&db, active.clone(), peak.clone(), Duration::ZERO).await
        },
    );
    held.expect("the first read");
    let err = waited.expect_err("no reader frees up within busy_timeout");
    assert!(err.to_string().contains("busy"), "{err}");
}

#[tokio::test]
async fn cancelled_read_keeps_its_slot_until_it_ends() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = open(&dir, 1, Duration::from_secs(5)).await;
    let (active, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));

    let slow = tokio::spawn({
        let (db, active, peak) = (db.clone(), active.clone(), peak.clone());
        async move { busy_read(&db, active, peak, Duration::from_millis(300)).await }
    });
    while active.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    slow.abort();
    busy_read(&db, active.clone(), peak.clone(), Duration::ZERO)
        .await
        .expect("the next read");
    assert_eq!(
        peak.load(Ordering::SeqCst),
        1,
        "the cancelled read still held the only connection"
    );
}
