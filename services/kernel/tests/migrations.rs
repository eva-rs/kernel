//! Миграции (`10 §5`, `§8`) и возможности встроенного SQLite (`10 §7`).

mod common;

use std::{num::NonZeroU64, path::Path, time::Duration};

use eva::time::Timestamp;
use eva_kernel::db::{
    Db as _, DbResult, Sqlite,
    migrate::{self, MIGRATIONS, MigrateError, Migration},
};
use rusqlite::Connection;

fn now() -> Timestamp {
    Timestamp::from_nanos(NonZeroU64::new(1_735_689_600_000_000_000).expect("non-zero"))
}

async fn memory() -> Sqlite {
    Sqlite::open(Path::new(":memory:"), Duration::from_secs(5), 1)
        .await
        .expect("open in-memory database")
}

async fn migrate(db: &Sqlite, migrations: &'static [Migration]) -> Result<usize, MigrateError> {
    db.exclusive(move |c| migrate::run(c, migrations, now()))
        .await
}

async fn query<T: Send + 'static>(
    db: &Sqlite,
    f: impl FnOnce(&Connection) -> DbResult<T> + Send + 'static,
) -> T {
    db.reader()
        .await
        .expect("reader")
        .run(f)
        .await
        .expect("query")
}

#[test]
fn embedded_migrations_are_numbered_without_gaps() {
    for (i, m) in MIGRATIONS.iter().enumerate() {
        assert_eq!(m.version as usize, i + 1, "{} is out of order", m.name);
    }
}

#[tokio::test]
async fn applies_on_empty_database_once() {
    let db = memory().await;
    assert_eq!(
        migrate(&db, MIGRATIONS).await.expect("migrate"),
        MIGRATIONS.len()
    );
    assert_eq!(migrate(&db, MIGRATIONS).await.expect("migrate again"), 0);

    let (versions, user_version, tables) = query(&db, |c| {
        let versions: i64 = c.query_row("SELECT count(*) FROM schema_migrations", [], |r| r.get(0))?;
        let user_version: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let mut stmt = c.prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?;
        let tables = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok((versions, user_version, tables))
    })
    .await;

    assert_eq!(versions, MIGRATIONS.len() as i64);
    assert_eq!(user_version, MIGRATIONS.len() as i64);
    for table in [
        "segments",
        "events",
        "messages",
        "tool_results",
        "token_ratios",
        "tools",
        "tool_groups",
        "memory",
        "memory_fts",
        "memory_archive",
        "chat_notes",
        "notes_state",
        "surfaces",
        "folders",
        "chats",
        "chat_aliases",
        "persons",
        "identities",
        "chat_speech",
        "chat_speech_fts",
        "chat_speech_vec",
        "modules",
        "module_services",
        "module_tokens",
        "module_kv",
        "agents",
        "agent_messages",
        "pending",
        "hands_grants",
        "chat_plan",
        "blobs",
        "cron_jobs",
        "cron_runs",
        "sleep_runs",
        "sleep_phase_runs",
        "gc_runs",
        "kernel_meta",
        "tool_stats",
        "chat_stats",
        "spend",
        "health_stats",
        "schema_migrations",
    ] {
        assert!(tables.iter().any(|t| t == table), "missing table {table}");
    }
}

#[tokio::test]
async fn refuses_unknown_applied_migration() {
    let db = memory().await;
    migrate(&db, MIGRATIONS).await.expect("migrate");
    db.write(|tx| -> DbResult<()> {
        tx.execute(
            "INSERT INTO schema_migrations (version, name, checksum, applied_at) VALUES (99, 'future', x'00', 1)",
            [],
        )?;
        Ok(())
    })
    .await
    .expect("insert future migration");

    let err = migrate(&db, MIGRATIONS).await.expect_err("must refuse");
    assert!(
        matches!(err, MigrateError::Unknown { version: 99, .. }),
        "{err}"
    );
}

#[tokio::test]
async fn refuses_changed_migration() {
    static CHANGED: &[Migration] = &[Migration {
        version: 1,
        name: "init",
        sql: "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, checksum BLOB NOT NULL, applied_at INTEGER NOT NULL) STRICT;",
    }];

    let db = memory().await;
    migrate(&db, MIGRATIONS).await.expect("migrate");
    let err = migrate(&db, CHANGED).await.expect_err("must refuse");
    assert!(
        matches!(err, MigrateError::Checksum { version: 1, .. }),
        "{err}"
    );
}

#[tokio::test]
async fn refuses_foreign_database() {
    let db = memory().await;
    db.write(|tx| -> DbResult<()> {
        tx.execute("CREATE TABLE stranger (x INTEGER)", [])?;
        Ok(())
    })
    .await
    .expect("create foreign table");

    let err = migrate(&db, MIGRATIONS).await.expect_err("must refuse");
    assert!(matches!(err, MigrateError::Foreign), "{err}");
}

#[tokio::test]
async fn kernel_refuses_to_start_on_unknown_migration() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = common::config(dir.path());
    let path = cfg.db_path();

    let db = Sqlite::open(&path, Duration::from_secs(5), 1)
        .await
        .expect("open");
    migrate(&db, MIGRATIONS).await.expect("migrate");
    let future = MIGRATIONS.len() as i64 + 1;
    db.write(move |tx| -> DbResult<()> {
        tx.execute(
            "INSERT INTO schema_migrations (version, name, checksum, applied_at) VALUES (?1, 'from_the_future', x'00', 1)",
            [future],
        )?;
        Ok(())
    })
    .await
    .expect("insert future migration");
    drop(db);

    let started = eva_kernel::Kernel::<common::Test>::start(
        cfg,
        eva_kernel::Parts {
            demiurge: eva_kernel::world::testing::Demiurge::new(1),
            db: common::FaultyDb::open(&path).await,
            providers: Default::default(),
            hands: Default::default(),
        },
    )
    .await;
    let err = started.err().expect("kernel must refuse the database");
    assert!(
        format!("{err:#}").contains("unknown to this build"),
        "{err:#}"
    );
}

/// FTS5 с trigram и математика salience работают во встроенной сборке
/// (`10 §7`) — на писателе и на читателе.
#[tokio::test]
async fn bundled_sqlite_has_trigram_fts_and_math() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = Sqlite::open(&dir.path().join("eva.sqlite"), Duration::from_secs(5), 1)
        .await
        .expect("open");
    migrate(&db, MIGRATIONS).await.expect("migrate");

    db.write(|tx| -> DbResult<()> {
        tx.execute(
            "INSERT INTO memory (kind, text, visibility, created_at, updated_at, valid_from, last_reinforced_at)
             VALUES ('fact', 'Master prefers green tea in the morning', 'public', 1, 1, 1, 1),
                    ('fact', 'The server lives in Helsinki', 'public', 1, 1, 1, 1)",
            [],
        )?;
        Ok(())
    })
    .await
    .expect("insert memory");

    let (hits, exp, ln, pow) = query(&db, |c| {
        let hits: Vec<i64> = c
            .prepare("SELECT rowid FROM memory_fts WHERE memory_fts MATCH 'elsin' ORDER BY rowid")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let (exp, ln, pow) = c.query_row("SELECT exp(0), ln(exp(2)), power(2, 10)", [], |r| {
            Ok((
                r.get::<_, f64>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, f64>(2)?,
            ))
        })?;
        Ok((hits, exp, ln, pow))
    })
    .await;

    assert_eq!(hits, vec![2], "trigram substring match");
    assert!((exp - 1.0).abs() < 1e-12);
    assert!((ln - 2.0).abs() < 1e-12);
    assert!((pow - 1024.0).abs() < 1e-12);

    let fk: i64 = db
        .write(|tx| -> DbResult<i64> { Ok(tx.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?) })
        .await
        .expect("writer pragma");
    assert_eq!(fk, 1, "writer enforces foreign keys");
    let fk: i64 = query(&db, |c| {
        Ok(c.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?)
    })
    .await;
    assert_eq!(fk, 1, "reader enforces foreign keys");
}

#[tokio::test]
async fn readers_cannot_write() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = Sqlite::open(&dir.path().join("eva.sqlite"), Duration::from_secs(5), 1)
        .await
        .expect("open");
    migrate(&db, MIGRATIONS).await.expect("migrate");

    let res = db
        .reader()
        .await
        .expect("reader")
        .run(|c| -> DbResult<usize> {
            Ok(c.execute("INSERT INTO kernel_meta VALUES ('k', 'v')", [])?)
        })
        .await;
    assert!(res.is_err(), "reader connection is query_only");
}

/// `0002` заводит счётчик KV и по строкам, записанным до него.
#[tokio::test]
async fn module_kv_usage_backfills_existing_rows() {
    let db = memory().await;
    let init: &'static [Migration] = MIGRATIONS.get(..1).expect("init migration");
    migrate(&db, init).await.expect("init");
    db.write(|tx| -> DbResult<()> {
        tx.execute_batch(
            "INSERT INTO module_kv (module, key, value, updated_at) VALUES
               ('svc', 'a', x'0102', 1), ('svc', 'ключ', x'', 1), ('other', 'b', x'00', 1)",
        )?;
        Ok(())
    })
    .await
    .expect("rows before 0002");
    let upto_kv: &'static [Migration] = MIGRATIONS.get(..2).expect("0002");
    assert_eq!(migrate(&db, upto_kv).await.expect("migrate"), 1);

    let usage: Vec<(String, i64)> = query(&db, |c| {
        let mut stmt = c.prepare("SELECT module, bytes FROM module_kv_usage ORDER BY module")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    })
    .await;
    assert_eq!(
        usage,
        [
            (String::from("other"), 1 + 1 + 64),
            (
                String::from("svc"),
                (1 + 2 + 64) + ("ключ".len() as i64 + 64)
            ),
        ]
    );
}

/// `0003`: токены сервисов отзываются (аудит `issued_by` остаётся), колонка
/// `kind` уходит, реестр `module_services` пуст.
#[tokio::test]
async fn module_services_revoke_service_tokens() {
    let db = memory().await;
    let upto_kv: &'static [Migration] = MIGRATIONS.get(..2).expect("0002");
    migrate(&db, upto_kv).await.expect("up to 0002");
    db.write(|tx| -> DbResult<()> {
        tx.execute_batch(
            "INSERT INTO module_tokens (token_hash, module, kind, issued_via, issued_at, revoked_at) VALUES
               (x'01', 'bridge', 'service', 'phone', 1, NULL),
               (x'02', 'tablet', 'client', 'phone', 1, NULL),
               (x'03', 'old', 'service', 'phone', 1, 5)",
        )?;
        Ok(())
    })
    .await
    .expect("tokens before 0003");
    assert_eq!(migrate(&db, MIGRATIONS).await.expect("migrate"), 1);

    let (tokens, columns, services) = query(&db, |c| {
        let mut stmt = c.prepare(
            "SELECT module, revoked_at IS NOT NULL, revoked_at FROM module_tokens ORDER BY module",
        )?;
        let tokens: Vec<(String, bool, Option<i64>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        let mut stmt = c.prepare("SELECT name FROM pragma_table_info('module_tokens')")?;
        let columns: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let services: i64 =
            c.query_row("SELECT count(*) FROM module_services", [], |r| r.get(0))?;
        Ok((tokens, columns, services))
    })
    .await;

    let [bridge, old, tablet] = tokens.as_slice() else {
        panic!("three tokens: {tokens:?}");
    };
    assert!(bridge.1, "a live service token is revoked: {bridge:?}");
    assert!(
        bridge.2.is_some_and(|at| at > 1_000_000_000_000_000_000),
        "revoked_at is a timestamp in ns: {bridge:?}"
    );
    assert_eq!(*old, (String::from("old"), true, Some(5)));
    assert_eq!(*tablet, (String::from("tablet"), false, None));
    assert!(!columns.iter().any(|c| c == "kind"), "{columns:?}");
    assert_eq!(services, 0);
}
