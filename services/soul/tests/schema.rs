//! Схема после миграций (`10 §8`): эталонный снимок `sqlite_master`, STRICT
//! на каждой таблице, CHECK-списки ⇔ варианты enum'ов, append-only `events`.

use std::{collections::BTreeSet, num::NonZeroU64, path::Path, time::Duration};

use eva::{_priv::schemars::JsonSchema, time::Timestamp};
use eva_soul::db::{
    Db as _, DbResult, Sqlite,
    migrate::{self, MIGRATIONS},
};
use rusqlite::Connection;
use serde_json::Value;

use eva_protocol::types::{
    Thinking, agent, blob, chat, hands, memory, module, note, sleep, surface, turn,
};

fn now() -> Timestamp {
    Timestamp::from_nanos(NonZeroU64::new(1_735_689_600_000_000_000).expect("non-zero"))
}

/// Пустой файл после всех миграций.
async fn migrated(dir: &Path) -> Sqlite {
    let db = Sqlite::open(&dir.join("eva.sqlite"), Duration::from_secs(5), 1)
        .await
        .expect("open database");
    db.exclusive(|c| migrate::run(c, MIGRATIONS, now()))
        .await
        .expect("migrate");
    db
}

async fn read<T: Send + 'static>(
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

/// SQL без `--`-комментариев и с одним пробелом вместо любых пробельных серий.
fn normalize(sql: &str) -> String {
    sql.lines()
        .map(|line| line.split_once("--").map_or(line, |(code, _)| code))
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug)]
struct Object {
    kind: String,
    name: String,
    table: String,
    sql: Option<String>,
}

async fn objects(db: &Sqlite) -> Vec<Object> {
    read(db, |c| {
        let mut stmt =
            c.prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Object {
                    kind: r.get(0)?,
                    name: r.get(1)?,
                    table: r.get(2)?,
                    sql: r.get::<_, Option<String>>(3)?.map(|s| normalize(&s)),
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    })
    .await
}

/// Обычные таблицы схемы и их флаг STRICT: без виртуальных, теневых FTS и `sqlite_*`.
fn tables(c: &Connection) -> DbResult<Vec<(String, bool)>> {
    let mut stmt = c.prepare(
        "SELECT name, strict FROM pragma_table_list
          WHERE schema = 'main' AND type = 'table' AND name NOT LIKE 'sqlite_%'
          ORDER BY name",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

#[tokio::test]
async fn reference_schema() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = migrated(dir.path()).await;

    let dump = objects(&db)
        .await
        .into_iter()
        .map(|o| {
            let sql = o.sql.as_deref().unwrap_or("-");
            format!("{} {} ({})\n  {sql}\n", o.kind, o.name, o.table)
        })
        .collect::<String>();
    insta::assert_snapshot!("schema", dump);
}

#[tokio::test]
async fn every_table_is_strict() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = migrated(dir.path()).await;

    let tables = read(&db, tables).await;
    assert!(tables.len() > 40, "only {} tables seen", tables.len());
    let loose: Vec<_> = tables.iter().filter(|(_, strict)| !strict).collect();
    assert!(loose.is_empty(), "tables without STRICT: {loose:?}");
}

/// Значение нужного типа для NOT NULL-колонки, чтобы вставка дошла до проверки типа.
fn filler(declared: &str) -> &'static str {
    match declared {
        "INTEGER" | "INT" => "0",
        "REAL" => "0.0",
        "BLOB" => "x''",
        _ => "''",
    }
}

#[tokio::test]
async fn text_never_lands_in_an_integer_column() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = migrated(dir.path()).await;

    let (probed, accepted) = db
        .exclusive(|c| -> DbResult<(usize, Vec<String>)> {
            let mut probed = 0;
            let mut accepted = Vec::new();
            for (table, _) in tables(c)? {
                let columns = {
                    let mut stmt =
                        c.prepare("SELECT name, type, \"notnull\" FROM pragma_table_info(?1)")?;
                    stmt.query_map([&table], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, bool>(2)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?
                };

                for (target, _, _) in columns.iter().filter(|(_, ty, _)| ty == "INTEGER") {
                    let (names, values): (Vec<_>, Vec<_>) = columns
                        .iter()
                        .filter(|(name, _, notnull)| name == target || *notnull)
                        .map(|(name, ty, _)| {
                            let value = if name == target {
                                "'not a number'"
                            } else {
                                filler(ty)
                            };
                            (format!("\"{name}\""), value)
                        })
                        .unzip();
                    let sql = format!(
                        "INSERT INTO \"{table}\" ({}) VALUES ({})",
                        names.join(", "),
                        values.join(", ")
                    );

                    probed += 1;
                    c.execute_batch("SAVEPOINT probe")?;
                    let res = c.execute(&sql, []);
                    c.execute_batch("ROLLBACK TO probe; RELEASE probe")?;
                    match res {
                        Err(e)
                            if e.to_string()
                                .contains("cannot store TEXT value in INTEGER column")
                                || e.to_string().contains("datatype mismatch") => {}
                        other => accepted.push(format!("{table}.{target}: {other:?}")),
                    }
                }
            }
            Ok((probed, accepted))
        })
        .await
        .expect("probe");
    assert!(probed > 100, "only {probed} integer columns probed");
    assert!(
        accepted.is_empty(),
        "text reached integer columns: {accepted:#?}"
    );
}

#[tokio::test]
async fn events_are_append_only() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = migrated(dir.path()).await;

    let err = db
        .exclusive(|c| -> DbResult<String> {
            // Триггер BEFORE UPDATE срабатывает только на существующей строке.
            c.execute_batch(
                "INSERT INTO surfaces (id, name, created_at) VALUES ('s', 's', 1);
                 INSERT INTO folders (surface_id, name) VALUES ('s', 'f');
                 INSERT INTO chats (folder_id, kind, created_at, last_activity_at)
                   SELECT id, 'persistent', 1, 1 FROM folders;",
            )?;
            let chat: i64 = c.query_row("SELECT id FROM chats", [], |r| r.get(0))?;
            c.execute(
                "INSERT INTO segments (chat_id, seq, started_at, reason, system_snapshot, tools_snapshot, hot_memory_ids)
                 VALUES (?1, 0, 1, 'initial', x'', x'', x'')",
                [chat],
            )?;
            c.execute(
                "INSERT INTO events (chat_id, segment_id, seq, kind, payload, created_at)
                 SELECT ?1, id, 0, 'message', x'', 1 FROM segments",
                [chat],
            )?;
            Ok(c.execute("UPDATE events SET payload = x'00'", [])
                .map(|n| format!("updated {n}"))
                .unwrap_or_else(|e| e.to_string()))
        })
        .await
        .expect("fixture");
    assert!(err.contains("events are append-only"), "{err}");
}

/// Список `IN (…)` из CHECK колонки в нормализованном SQL таблицы.
fn check_list(sql: &str, column: &str) -> Option<BTreeSet<String>> {
    let head = format!("CHECK ({column} IN (");
    let start = sql.find(&head)? + head.len();
    let end = start + sql[start..].find(')')?;
    Some(
        sql[start..end]
            .split(',')
            .map(|v| v.trim().trim_matches('\'').to_owned())
            .collect(),
    )
}

/// Имена вариантов на проводе — из JSON Schema типа (`enum` либо `oneOf` констант).
fn variants<T: JsonSchema>() -> BTreeSet<String> {
    fn collect(schema: &Value, out: &mut BTreeSet<String>) {
        if let Some(Value::Array(values)) = schema.get("enum") {
            out.extend(values.iter().filter_map(Value::as_str).map(str::to_owned));
        }
        if let Some(Value::String(value)) = schema.get("const") {
            out.insert(value.clone());
        }
        for key in ["oneOf", "anyOf"] {
            if let Some(Value::Array(branches)) = schema.get(key) {
                branches.iter().for_each(|b| collect(b, out));
            }
        }
    }

    let schema = eva::_priv::schemars::SchemaGenerator::default().into_root_schema_for::<T>();
    let schema = serde_json::to_value(&schema).expect("schema is json");
    let mut out = BTreeSet::new();
    collect(&schema, &mut out);
    assert!(
        !out.is_empty(),
        "no variants in the schema of {}",
        std::any::type_name::<T>()
    );
    out
}

/// Enum ↔ колонка под CHECK: одна строка на пару.
struct Pair {
    table: &'static str,
    column: &'static str,
    variants: fn() -> BTreeSet<String>,
}

const fn pair(
    table: &'static str,
    column: &'static str,
    variants: fn() -> BTreeSet<String>,
) -> Pair {
    Pair {
        table,
        column,
        variants,
    }
}

const PAIRS: &[Pair] = &[
    pair("segments", "reason", variants::<chat::SegmentReason>),
    pair("messages", "role", variants::<chat::Role>),
    pair("memory", "kind", variants::<memory::Kind>),
    pair("memory", "visibility", variants::<memory::Visibility>),
    pair(
        "memory_archive",
        "reason",
        variants::<memory::ArchiveReason>,
    ),
    pair("notes_state", "mode", variants::<note::Mode>),
    pair("surfaces", "thinking", variants::<Thinking>),
    pair("surfaces", "builtin", variants::<surface::Builtin>),
    pair("folders", "thinking", variants::<Thinking>),
    pair("chats", "kind", variants::<chat::Kind>),
    pair("chats", "thinking", variants::<Thinking>),
    pair(
        "chat_aliases",
        "external_kind",
        variants::<chat::ExternalKind>,
    ),
    pair("modules", "state", variants::<module::State>),
    pair("agents", "status", variants::<agent::Status>),
    pair("pending", "kind", variants::<turn::PendingKind>),
    pair("pending", "outcome", variants::<turn::PendingOutcome>),
    pair("hands_grants", "kind", variants::<hands::GrantKind>),
    pair("chat_plan", "status", variants::<turn::TaskStatus>),
    pair("blobs", "kind", variants::<blob::Kind>),
    pair("blobs", "source", variants::<eva_soul::db::blobs::Source>),
    pair("sleep_phase_runs", "phase", variants::<sleep::Phase>),
    pair(
        "spend",
        "cache_break_reason",
        variants::<eva_protocol::types::stats::BreakReason>,
    ),
];

#[tokio::test]
async fn check_lists_equal_enum_variants() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db = migrated(dir.path()).await;
    let objects = objects(&db).await;

    let mut mismatches = Vec::new();
    for Pair {
        table,
        column,
        variants,
    } in PAIRS
    {
        let sql = objects
            .iter()
            .find(|o| o.kind == "table" && o.name == *table)
            .and_then(|o| o.sql.as_deref())
            .unwrap_or_else(|| panic!("no table {table}"));
        let check =
            check_list(sql, column).unwrap_or_else(|| panic!("no CHECK IN on {table}.{column}"));

        let mut wire = variants();
        // Терпимый catch-all пира в БД не пишется.
        if !check.contains("unknown") {
            wire.remove("unknown");
        }
        if wire != check {
            mismatches.push(format!(
                "{table}.{column}: enum {wire:?} != CHECK {check:?}"
            ));
        }
    }
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}

/// `events` чистится только каскадом purge (`10 §8`).
#[test]
fn no_direct_delete_from_events() {
    fn walk(dir: &Path, hits: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("read dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, hits);
            } else if let Ok(text) = std::fs::read_to_string(&path) {
                let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
                if flat.to_ascii_uppercase().contains("DELETE FROM EVENTS") {
                    hits.push(path.display().to_string());
                }
            }
        }
    }

    let mut hits = Vec::new();
    walk(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut hits,
    );
    assert!(hits.is_empty(), "DELETE FROM events in {hits:?}");
}
