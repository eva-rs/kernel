//! Компонент `gc` (`08 §3`): еженедельный слот, одна транзакция уборки с
//! каскадом подагентов и крон-джоб, blob'ы открытого сегмента не трогаются,
//! ретеншн журналов и статистики, откат целиком при ошибке.

mod common;

use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use common::Harness;
use eva::time::Clock as _;
use eva_kernel::{
    db::{Db as _, DbResult},
    subsystems::{TurnCancel, gc::Gc},
};
use eva_protocol::{
    types::ChatId,
    wire::events::{ChatPurged, Event},
};

const SEC: i64 = 1_000_000_000;
const DAY: i64 = 86_400 * SEC;

/// Движок туров теста: записывает отменённые чаты.
#[derive(Clone, Default)]
struct Cancels(Arc<Mutex<Vec<ChatId>>>);

impl TurnCancel for Cancels {
    async fn cancel(&self, chats: Vec<ChatId>) {
        self.0.lock().expect("cancels").extend(chats);
    }
}

async fn start() -> Harness {
    common::start_with(|cfg| {
        cfg.system.tz = Some(String::from("UTC"));
        cfg.gc.cron_runs_keep = 2;
        cfg.gc.sleep_runs_keep = 1;
        cfg.gc.gc_runs_keep = 2;
    })
    .await
}

async fn exec(h: &Harness, sql: String) {
    h.kernel
        .shared()
        .db()
        .write(move |tx| -> DbResult<()> {
            tx.execute_batch(&sql)?;
            Ok(())
        })
        .await
        .expect("seed");
}

fn t0(h: &Harness) -> i64 {
    i64::try_from(h.clock.get().as_nanos().get()).expect("nanos fit")
}

/// Чат в папке `surface/folder`; `deleted` — `deleted_at`.
fn chat(
    id: i64,
    place: (&str, &str),
    kind: &str,
    activity: i64,
    deleted: Option<i64>,
    undeletable: bool,
) -> String {
    let (surface, folder) = place;
    let deleted = deleted.map_or(String::from("NULL"), |t| t.to_string());
    let undeletable = i32::from(undeletable);
    format!(
        "INSERT INTO chats (id, folder_id, kind, created_at, last_activity_at, deleted_at, undeletable)
           SELECT {id}, id, '{kind}', {activity}, {activity}, {deleted}, {undeletable} FROM folders
            WHERE surface_id = '{surface}' AND name = '{folder}';"
    )
}

/// Жертвы и выжившие каждой цели `08 §3.2`.
async fn seed(h: &Harness) {
    let t = t0(h);
    let unsorted = ("local", "Unsorted");
    // 100 — idle ephemeral с подагентом 101, 103 — помечен два года назад и держит джобу.
    let mut sql = [
        chat(100, unsorted, "ephemeral", t - 3 * DAY, None, false),
        chat(101, ("local", "agents"), "persistent", t, None, false),
        chat(102, unsorted, "ephemeral", t - DAY, None, false),
        chat(
            103,
            unsorted,
            "persistent",
            t - 800 * DAY,
            Some(t - 730 * DAY),
            false,
        ),
        chat(
            104,
            ("system", "cron"),
            "persistent",
            t - 800 * DAY,
            None,
            false,
        ),
        chat(
            105,
            ("system", "sleep"),
            "persistent",
            t - 800 * DAY,
            None,
            true,
        ),
        chat(
            106,
            unsorted,
            "persistent",
            t - 10 * DAY,
            Some(t - 10 * DAY),
            false,
        ),
    ]
    .concat();
    sql += &format!(
        "INSERT INTO agents (id, parent_chat_id, name, task, tools, master_turn, status, started_at)
           VALUES (101, 100, 'scout', 'look around', '[]', 1, 'running', {t});
         INSERT INTO cron_jobs (id, name, task, interval, next_fire_at, chat_id, created_at)
           VALUES ('job-dead', 'dead', 'x', NULL, NULL, 103, {t}),
                  ('job-live', 'live', 'y', {hour}, {next}, 104, {t});
         INSERT INTO blobs (id, kind, chat_id, name, media_type, size, hash, source, created_at, expires_at) VALUES
           ('blob-gone', 'file', 102, 'a.txt', 'text/plain', 1, x'00', 'upload', {old}, {past}),
           ('blob-seen', 'file', 102, 'b.txt', 'text/plain', 1, x'00', 'upload', {old}, {past}),
           ('blob-live', 'file', 102, 'c.txt', 'text/plain', 1, x'00', 'upload', {old}, {future});
         INSERT INTO segments (id, chat_id, seq, started_at, reason, system_snapshot, tools_snapshot, hot_memory_ids)
           VALUES (1, 102, 1, {t}, 'initial', x'', x'', x'');
         INSERT INTO events (chat_id, segment_id, seq, kind, payload, created_at)
           VALUES (102, 1, 1, 'message', CAST('the file blob-seen is still in context' AS BLOB), {t});
         INSERT INTO memory (id, kind, text, visibility, created_at, updated_at, valid_from, valid_to,
                             last_reinforced_at, supersedes, version) VALUES
           (1, 'fact', 'forgotten long ago', 'private', {d100}, {d100}, {d100}, {d40}, {d100}, NULL, 1),
           (2, 'fact', 'forgotten recently', 'private', {d100}, {d100}, {d100}, {d10}, {d100}, NULL, 1),
           (3, 'fact', 'restored', 'private', {d100}, {d100}, {d100}, NULL, {d100}, NULL, 1),
           (4, 'fact', 'old version', 'private', {d800}, {d800}, {d800}, {d400}, {d800}, NULL, 1),
           (5, 'fact', 'new version', 'private', {d400}, {d400}, {d400}, NULL, {d400}, 4, 2),
           (6, 'fact', 'young old version', 'private', {d100}, {d100}, {d100}, {d10}, {d100}, NULL, 1),
           (7, 'fact', 'its successor', 'private', {d10}, {d10}, {d10}, NULL, {d10}, 6, 2);
         INSERT INTO memory_archive (memory_id, reason, why, archived_at, restored_at) VALUES
           (1, 'forget', 'stale', {d40}, NULL), (2, 'forget', 'stale', {d10}, NULL), (3, 'forget', 'oops', {d40}, {d10});
         INSERT INTO chat_notes (chat_id, name, description, body, created_at, updated_at, deleted_at) VALUES
           (102, 'old', 'gone', 'body', {d100}, {d100}, {d40}), (102, 'fresh', 'kept', 'body', {d100}, {d100}, {d10});
         INSERT INTO tools (tool_id, module, path, canonical, decl, spec_hash, registered_at, last_seen_at)
           VALUES (10000, 'kernel', 'fixture.read', 'fixture_read', '{{}}', x'00', {t}, {t});
         INSERT INTO tool_pins (chat_id, tool_id, until) VALUES (102, 10000, {past}), (104, 10000, {future});
         INSERT INTO embed_cache (text_hash, embed_model, embedding, created_at) VALUES
           (x'01', 'm', x'00', {d100}), (x'02', 'm', x'00', {d10});
         INSERT INTO pending (id, chat_id, turn_id, call_id, kind, payload, created_at, expires_at, resolved_at, outcome) VALUES
           ('p-old', 102, 'tr', 'c1', 'ask', '{{}}', {d40}, {d40}, {d40}, 'answered'),
           ('p-new', 102, 'tr', 'c2', 'ask', '{{}}', {d10}, {d10}, {d10}, 'answered'),
           ('p-open', 102, 'tr', 'c3', 'ask', '{{}}', {t}, {future}, NULL, NULL);
         INSERT INTO tool_stats (day, chat_id, tool_id, calls, last_at) VALUES ('2023-01-01', 102, 10000, 1, {d800}), ('2024-12-31', 102, 10000, 1, {t});
         INSERT INTO chat_stats (day, chat_id, turns) VALUES ('2023-01-01', 102, 1), ('2024-12-31', 102, 1);
         INSERT INTO health_stats (day, metric, key, count) VALUES ('2023-01-01', 'cron.ok', 'a', 1), ('2024-12-31', 'cron.ok', 'a', 1);
         INSERT INTO spend (ts, model, source, op, input, cache_read, cache_write, output, cost_source) VALUES
           ({y4}, 'mock-model', 'surface', 'step', 1, 0, 0, 1, 'none'), ({d10}, 'mock-model', 'surface', 'step', 1, 0, 0, 1, 'none');
         INSERT INTO cron_runs (run_id, job_id, started) VALUES ('c1', 'job-live', {d40}), ('c2', 'job-live', {d10}), ('c3', 'job-live', {t});
         INSERT INTO sleep_runs (run_id, started, dry_run) VALUES ('s1', {d40}, 0), ('s2', {d10}, 0);
         INSERT INTO sleep_phase_runs (run_id, phase, started) VALUES ('s1', 'memory', {d40}), ('s2', 'memory', {d10});
         INSERT INTO gc_runs (run_id, started, finished, ok, deleted_json) VALUES
           ('g1', {d40}, {d40}, 1, '{{}}'), ('g2', {d10}, {d10}, 1, '{{}}');",
        hour = 3600 * SEC,
        next = t + 3600 * SEC,
        old = t - 10 * DAY,
        past = t - DAY,
        future = t + DAY,
        d10 = t - 10 * DAY,
        d40 = t - 40 * DAY,
        d100 = t - 100 * DAY,
        d400 = t - 400 * DAY,
        d800 = t - 800 * DAY,
        y4 = t - 4 * 365 * DAY,
    );
    exec(h, sql).await;
}

async fn ids(h: &Harness, sql: &'static str) -> Vec<String> {
    h.query(move |c| {
        let mut stmt = c.prepare(sql)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, rusqlite::types::Value>(0))?
            .map(|v| {
                v.map(|v| match v {
                    rusqlite::types::Value::Integer(n) => n.to_string(),
                    rusqlite::types::Value::Text(s) => s,
                    other => format!("{other:?}"),
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(rows)
    })
    .await
}

fn blob_dir(h: &Harness) -> std::path::PathBuf {
    h.kernel.shared().cfg().blob_dir()
}

fn make_dir(path: &std::path::Path, age: Option<Duration>) {
    std::fs::create_dir_all(path).expect("mkdir");
    if let Some(age) = age {
        std::fs::File::open(path)
            .and_then(|f| f.set_modified(SystemTime::now() - age))
            .expect("set mtime");
    }
}

#[tokio::test]
async fn one_sweep_purges_every_target_and_spares_the_living() {
    let h = start().await;
    seed(&h).await;
    let dir = blob_dir(&h);
    for name in ["blob-gone", "blob-seen", "blob-live"] {
        make_dir(&dir.join(name), None);
    }
    make_dir(&dir.join("orphan"), Some(Duration::from_secs(3 * 86_400)));
    make_dir(&dir.join("fresh-orphan"), None);
    make_dir(&dir.join(".staging"), Some(Duration::from_secs(3 * 86_400)));
    let cancels = Cancels::default();
    let gc = Gc::new(h.kernel.clone(), cancels.clone());
    let mut events = h.kernel.shared().bus().subscribe();

    let report = gc.tick().await.expect("tick").expect("first tick is due");

    assert_eq!(
        report.chats,
        [ChatId(100), ChatId(101), ChatId(103)],
        "subagent goes with its lead"
    );
    assert_eq!(
        ids(&h, "SELECT id FROM chats WHERE id >= 100 ORDER BY id").await,
        ["102", "104", "105", "106"]
    );
    assert_eq!(
        ids(&h, "SELECT id FROM cron_jobs ORDER BY id").await,
        ["job-live"],
        "the job goes with its chat"
    );
    assert_eq!(ids(&h, "SELECT id FROM agents").await, Vec::<String>::new());
    assert_eq!(
        ids(&h, "SELECT id FROM blobs ORDER BY id").await,
        ["blob-live", "blob-seen"],
        "an open segment still shows blob-seen"
    );
    assert_eq!(
        ids(&h, "SELECT id FROM memory ORDER BY id").await,
        ["2", "3", "5", "6", "7"]
    );
    assert_eq!(
        ids(
            &h,
            "SELECT memory_id FROM memory_archive ORDER BY memory_id"
        )
        .await,
        ["2", "3"]
    );
    assert_eq!(ids(&h, "SELECT name FROM chat_notes").await, ["fresh"]);
    assert_eq!(ids(&h, "SELECT chat_id FROM tool_pins").await, ["104"]);
    assert_eq!(
        ids(&h, "SELECT hex(text_hash) FROM embed_cache").await,
        ["02"]
    );
    assert_eq!(
        ids(&h, "SELECT id FROM pending ORDER BY id").await,
        ["p-new", "p-open"]
    );
    for table in [
        "SELECT day FROM tool_stats",
        "SELECT day FROM chat_stats",
        "SELECT day FROM health_stats WHERE metric = 'cron.ok'",
    ] {
        assert_eq!(ids(&h, table).await, ["2024-12-31"], "{table}");
    }
    assert_eq!(ids(&h, "SELECT COUNT(*) FROM spend").await, ["1"]);
    assert_eq!(
        ids(&h, "SELECT run_id FROM cron_runs ORDER BY run_id").await,
        ["c2", "c3"]
    );
    assert_eq!(ids(&h, "SELECT run_id FROM sleep_runs").await, ["s2"]);
    assert_eq!(
        ids(&h, "SELECT run_id FROM sleep_phase_runs").await,
        ["s2"],
        "phases cascade"
    );
    let gc_runs = ids(&h, "SELECT run_id FROM gc_runs ORDER BY started").await;
    assert_eq!(gc_runs, ["g2", report.run.as_str()]);
    assert!(ids(&h, "PRAGMA foreign_key_check").await.is_empty());

    assert_eq!(report.deleted.get("chats"), Some(&3));
    assert_eq!(report.deleted.get("memory_forgotten"), Some(&1));
    assert_eq!(report.deleted.get("memory_superseded"), Some(&1));
    let json: serde_json::Value = serde_json::from_str(
        &ids(
            &h,
            "SELECT deleted_json FROM gc_runs WHERE ok = 1 ORDER BY started DESC LIMIT 1",
        )
        .await[0],
    )
    .expect("deleted_json");
    assert_eq!(json["blobs"], 1);
    assert_eq!(
        h.counter("gc.purged").await,
        report.deleted.values().sum::<u64>() as i64
    );

    assert!(!dir.join("blob-gone").exists());
    assert!(!dir.join("orphan").exists());
    for kept in ["blob-seen", "blob-live", "fresh-orphan", ".staging"] {
        assert!(dir.join(kept).exists(), "{kept}");
    }
    assert_eq!(
        *cancels.0.lock().expect("cancels"),
        report.chats,
        "live turns of victims get cancelled"
    );
    let purged = events.try_recv().expect("chat.purged");
    assert!(
        matches!(purged.event, Event::ChatPurged(ChatPurged { ref ids }) if ids == &report.chats),
        "{purged:?}"
    );
    assert_eq!(purged.surface.map(|s| s.id.0), Some(String::from("local")));

    assert!(gc.tick().await.expect("tick").is_none(), "the slot is done");
}

#[tokio::test]
async fn a_failure_mid_transaction_changes_nothing_but_the_journal() {
    let h = start().await;
    seed(&h).await;
    exec(
        &h,
        String::from(
            "CREATE TRIGGER gc_boom BEFORE INSERT ON gc_runs WHEN new.ok = 1
               BEGIN SELECT RAISE(ABORT, 'boom'); END;",
        ),
    )
    .await;
    const DUMP: &str = "SELECT (SELECT group_concat(id || ':' || COALESCE(deleted_at, 'null') ORDER BY id) FROM chats) || '|' || (SELECT group_concat(id ORDER BY id) FROM blobs)
                          || '|' || (SELECT group_concat(id || ':' || status || ':' || COALESCE(finished_at, 'null') ORDER BY id) FROM agents)
                          || '|' || (SELECT group_concat(id ORDER BY id) FROM memory) || '|' || (SELECT COUNT(*) FROM spend)
                          || '|' || (SELECT group_concat(run_id ORDER BY run_id) FROM cron_runs) || '|' || (SELECT COUNT(*) FROM health_stats)";
    let before = ids(&h, DUMP).await;
    let gc = Gc::new(h.kernel.clone(), Cancels::default());

    assert!(gc.collect().await.is_err());

    assert_eq!(ids(&h, DUMP).await, before, "the sweep rolled back whole");
    assert_eq!(
        ids(
            &h,
            "SELECT ok FROM gc_runs WHERE run_id NOT IN ('g1', 'g2')"
        )
        .await,
        ["0"]
    );
    assert!(
        gc.tick().await.expect("tick").is_none(),
        "a failed slot waits for the next one"
    );
}

#[tokio::test]
async fn the_weekly_slot_fires_once_per_week() {
    let h = start().await;
    let gc = Gc::new(h.kernel.clone(), Cancels::default());

    assert!(
        gc.tick().await.expect("tick").is_some(),
        "the slot missed during downtime"
    );
    assert!(gc.tick().await.expect("tick").is_none());
    // 2025-01-01 (Wed) 00:00Z → следующий слот — воскресенье 03:00.
    let mut clock = h.clock.clone();
    clock.advance(Duration::from_secs(4 * 86_400 + 2 * 3600));
    assert!(
        gc.tick().await.expect("tick").is_none(),
        "sunday 02:00 is before the slot"
    );
    clock.advance(Duration::from_secs(3600));
    assert!(gc.tick().await.expect("tick").is_some(), "sunday 03:00");
    assert!(gc.tick().await.expect("tick").is_none());
}
