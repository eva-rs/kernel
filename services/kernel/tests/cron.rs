//! `cron.*` (`08 §1.7`): джобы с актуальными канонами whitelist, журнал
//! запусков keyset'ом по `(started, run_id)`, матрица `Policy`.

mod common;

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use common::{Harness, Outcome, outcome};
use eva::{handling::CallStep as _, time::Clock as _};
use eva_kernel::{
    db::{Db as _, DbResult},
    subsystems::{
        CronTurns,
        cron::{self, Cron, Outcome as TurnOutcome, TurnRequest},
    },
};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::{
        Cron::{Trait as _, list, runs},
        RunsLimit,
        Stats::health,
    },
    types::{CronJobId, Effort, RunId},
    wire::events::Event,
};
use tokio::sync::Notify;

const SEC: i64 = 1_000_000_000;

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

/// Два чата `local` и две джобы: `daily` с whitelist из живого, ретированного
/// и пропавшего тула, `once` — one-shot без ограничений.
async fn seed_jobs(h: &Harness, tools: &str) {
    let t = t0(h);
    exec(
        h,
        format!(
            "INSERT INTO chats (id, folder_id, kind, created_at, last_activity_at)
               SELECT 10, id, 'persistent', {t}, {t} FROM folders WHERE surface_id = 'local' AND name = 'Unsorted';
             INSERT INTO chats (id, folder_id, kind, created_at, last_activity_at)
               SELECT 11, id, 'persistent', {t}, {t} FROM folders WHERE surface_id = 'local' AND name = 'Unsorted';
             INSERT INTO tools (tool_id, module, path, canonical, decl, spec_hash, registered_at, last_seen_at, retired_at)
               VALUES (10001, 'kernel', 'fixture.read', 'fixture_read', '{{}}', x'00', {t}, {t}, NULL),
                      (10002, 'kernel', 'fixture.old', 'fixture_old', '{{}}', x'01', {t}, {t}, {t});
             INSERT INTO cron_jobs (id, name, task, interval, next_fire_at, expires_at, model, effort, tools,
                                    chat_id, created_at, runs_total, spent_total)
               VALUES ('job-b', 'once', 'ping master', NULL, NULL, NULL, NULL, NULL, NULL, 11, {late}, 1, 0.0),
                      ('job-a', 'daily', 'summarize the day', {interval}, {next}, {expires}, 'mock-model', 3,
                       '{tools}', 10, {t}, 4, 0.25);",
            late = t + SEC,
            interval = 86_400 * SEC,
            next = t + 3600 * SEC,
            expires = t + 30 * 86_400 * SEC,
        ),
    )
    .await;
}

#[tokio::test]
async fn list_shows_jobs_with_current_tool_names() {
    let h = common::start().await;
    seed_jobs(&h, "[10001, 10002, 10099]").await;

    let list::Ok { jobs } = h
        .client_session()
        .await
        .cron()
        .list()
        .await
        .expect("cron.list");
    let names: Vec<_> = jobs.iter().map(|j| j.name.as_str()).collect();
    assert_eq!(names, ["daily", "once"], "by created_at");

    let daily = &jobs[0];
    assert_eq!(daily.id, CronJobId::from("job-a"));
    assert_eq!(daily.task, "summarize the day");
    assert_eq!(
        daily.interval.map(|i| i.0),
        Some(Duration::from_secs(86_400))
    );
    assert_eq!(
        daily.next_fire_at.map(|t| t.as_nanos().get()),
        Some(h.clock.get().as_nanos().get() + 3600 * SEC as u64)
    );
    assert!(daily.expires_at.is_some());
    assert_eq!(
        daily.model.as_ref().map(|m| m.as_str()),
        Some(common::MODEL)
    );
    assert_eq!(daily.effort, Effort::from_u8(3));
    assert_eq!(
        daily.tools.as_deref(),
        Some(
            &[
                String::from("fixture_read"),
                String::from("fixture_old"),
                String::from("tool10099")
            ][..]
        )
    );
    assert_eq!(daily.chat.get(), 10);
    assert_eq!(daily.runs_total, 4);
    assert!((daily.spent_total - 0.25).abs() < 1e-9);

    let once = &jobs[1];
    assert_eq!(once.interval, None, "one-shot");
    assert_eq!(once.next_fire_at, None, "one-shot already fired");
    assert_eq!(once.tools, None, "unrestricted");
}

#[tokio::test]
async fn corrupt_whitelist_is_a_db_error() {
    let h = common::start().await;
    seed_jobs(&h, "not json").await;
    let res = h.client_session().await.cron().list().await;
    assert!(matches!(res, Err(Generic::Aux(Aux::Db(_)))), "{res:?}");
}

/// Запуски: `r1..r5`, у `r3`/`r4` одинаковый `started` — порядок решает `run_id`.
async fn seed_runs(h: &Harness) {
    let t = t0(h);
    exec(
        h,
        format!(
            "INSERT INTO cron_runs (run_id, job_id, started, finished, ok, output, cost) VALUES
               ('r1', 'job-a', {t1}, {t1}, 1, 'done', 0.01),
               ('r2', 'job-b', {t2}, NULL, NULL, NULL, NULL),
               ('r3', 'job-a', {t3}, {t3}, 0, '[cron skipped] previous run alive', NULL),
               ('r4', 'job-a', {t3}, {t3}, 1, 'done', 0.02),
               ('r5', 'gone-one-shot', {t5}, {t5}, 1, 'bye', 0.03);",
            t1 = t + SEC,
            t2 = t + 2 * SEC,
            t3 = t + 3 * SEC,
            t5 = t + 5 * SEC,
        ),
    )
    .await;
}

async fn page(
    h: &Harness,
    job: Option<&str>,
    limit: u16,
    after: Option<&str>,
) -> Result<Vec<String>, Generic<runs::Err>> {
    let runs::Ok { found } = h
        .client_session()
        .await
        .cron()
        .runs(
            job.map(CronJobId::from),
            RunsLimit::new(limit).expect("limit in range"),
            after.map(RunId::from),
        )
        .await?;
    Ok(found.into_iter().map(|r| r.run_id.0).collect())
}

#[tokio::test]
async fn runs_newest_first_with_keyset_pages() {
    let h = common::start().await;
    seed_runs(&h).await;

    let all = page(&h, None, 20, None).await.expect("all");
    assert_eq!(all, ["r5", "r4", "r3", "r2", "r1"]);

    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let found = page(&h, None, 2, after.as_deref()).await.expect("page");
        seen.extend(found.iter().cloned());
        if found.len() < 2 {
            break;
        }
        after = found.last().cloned();
    }
    assert_eq!(seen, all, "pages neither repeat nor skip rows");

    assert_eq!(
        page(&h, Some("job-a"), 20, None).await.expect("job"),
        ["r4", "r3", "r1"]
    );
    assert_eq!(
        page(&h, Some("job-a"), 20, Some("r4"))
            .await
            .expect("job after"),
        ["r3", "r1"]
    );
    assert_eq!(
        page(&h, Some("gone-one-shot"), 20, None)
            .await
            .expect("deleted job"),
        ["r5"],
        "runs outlive a deleted one-shot"
    );
}

#[tokio::test]
async fn runs_row_fields_and_unknown_cursor() {
    let h = common::start().await;
    seed_runs(&h).await;

    let runs::Ok { found } = h
        .client_session()
        .await
        .cron()
        .inner_mut()
        .runs()
        .call(runs::Args {
            job: Some(CronJobId::from("job-b")),
            ..Default::default()
        })
        .await
        .expect("runs");
    let r2 = &found[0];
    assert_eq!(r2.job, CronJobId::from("job-b"));
    assert_eq!(
        (r2.finished, r2.ok, r2.output.as_deref(), r2.cost),
        (None, None, None, None)
    );

    let res = page(&h, None, 20, Some("nope")).await;
    assert!(
        matches!(res, Err(Generic::Aux(Aux::NotFound(_)))),
        "{res:?}"
    );
}

#[tokio::test]
async fn cron_matrix() {
    use Outcome::{Forbidden as F, Ok as K};

    let h = common::start().await;
    seed_jobs(&h, "[1]").await;
    seed_runs(&h).await;
    h.service_with_surface(common::SVC).await;
    h.service_with_surface(common::OTHER).await;

    let mut failures = Vec::new();
    for ((who, caller), want) in common::matrix_callers().into_iter().zip([K, K, F, F]) {
        let mut s = h.session(caller).await;
        let listed = outcome(&s.cron().list().await);
        let runs = outcome(
            &s.cron()
                .inner_mut()
                .runs()
                .call(runs::Args::default())
                .await,
        );
        for (op, got) in [("list", listed), ("runs", runs)] {
            if got != want {
                failures.push(format!("cron.{op} as {who}: want {want:?}, got {got:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// --- компонент `cron` (`08 §1.2`, `§1.3`, `§1.6`) ---

/// Движок туров теста: записывает запросы, отвечает по сценарию; `gate` держит тур открытым.
#[derive(Clone, Default)]
struct Recording {
    seen: Arc<Mutex<Vec<TurnRequest>>>,
    outcome: Arc<Mutex<Option<TurnOutcome>>>,
    gate: Arc<Mutex<Option<Arc<Notify>>>>,
}

impl CronTurns for Recording {
    async fn run(&self, req: TurnRequest) -> TurnOutcome {
        self.seen.lock().expect("seen").push(req);
        let gate = self.gate.lock().expect("gate").clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        let scripted = self.outcome.lock().expect("outcome").clone();
        scripted.unwrap_or(TurnOutcome::Ok {
            output: Some(String::from("done")),
        })
    }
}

fn hour() -> i64 {
    3600 * SEC
}

/// Чат 10 на `local` и джоба `daily` с заданным расписанием.
async fn seed_job(
    h: &Harness,
    next: Option<i64>,
    interval: Option<i64>,
    pending: &str,
    tools: &str,
) {
    let t = t0(h);
    let opt = |v: Option<i64>| v.map_or(String::from("NULL"), |v| v.to_string());
    exec(
        h,
        format!(
            "INSERT INTO chats (id, folder_id, kind, created_at, last_activity_at)
               SELECT 10, id, 'persistent', {t}, {t} FROM folders WHERE surface_id = 'local' AND name = 'Unsorted';
             INSERT INTO cron_jobs (id, name, task, interval, next_fire_at, fire_pending, tools, chat_id, created_at, runs_total)
               VALUES ('job-a', 'daily', 'summarize the day', {interval}, {next}, {pending}, {tools}, 10, {t}, 4);",
            interval = opt(interval),
            next = opt(next),
        ),
    )
    .await;
}

async fn job_next(h: &Harness) -> Option<i64> {
    h.query(|c| {
        Ok(c.query_row(
            "SELECT next_fire_at FROM cron_jobs WHERE id = 'job-a'",
            [],
            |r| r.get(0),
        )?)
    })
    .await
}

/// `(finished, ok, output, due)` единственного запуска.
async fn single_run(h: &Harness) -> (Option<i64>, Option<bool>, Option<String>, Option<i64>) {
    h.query(|c| {
        Ok(
            c.query_row("SELECT finished, ok, output, due FROM cron_runs", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?,
        )
    })
    .await
}

#[tokio::test]
async fn due_job_fires_once_and_the_slot_moves_before_the_turn() {
    let h = common::start().await;
    let t = t0(&h);
    seed_job(&h, Some(t), Some(hour()), "NULL", "NULL").await;
    let engine = Recording::default();
    let cron = Cron::new(h.kernel.clone(), engine.clone());
    let mut events = h.kernel.shared().bus().subscribe();

    let tick = cron.tick().await.expect("tick");
    assert_eq!(tick.fired.len(), 1);
    assert_eq!(
        job_next(&h).await,
        Some(t + hour()),
        "slot moved before the turn ended"
    );
    cron.settle().await;

    let seen = engine.seen.lock().expect("seen").clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].message, "[cron daily fired] summarize the day");
    assert_eq!((seen[0].chat.get(), seen[0].tools.as_ref()), (10, None));
    assert_eq!(seen[0].run, tick.fired[0]);
    assert_eq!(
        single_run(&h).await,
        (Some(t), Some(true), Some(String::from("done")), Some(t))
    );
    let runs_total: i64 = h
        .query(|c| Ok(c.query_row("SELECT runs_total FROM cron_jobs", [], |r| r.get(0))?))
        .await;
    assert_eq!(runs_total, 5);
    assert_eq!(h.counter("cron.ok").await, 1);
    assert!(
        matches!(events.try_recv(), Ok(p) if matches!(p.event, Event::CronFired(ref f) if f.run_id == tick.fired[0] && f.chat.get() == 10)),
        "cron.fired goes to the bus"
    );

    let again = cron.tick().await.expect("tick");
    assert!(again.fired.is_empty(), "the next slot is an hour away");
}

#[tokio::test]
async fn a_live_turn_skips_the_slot_but_the_schedule_still_moves() {
    let h = common::start().await;
    let t = t0(&h);
    seed_job(&h, Some(t), Some(hour()), "NULL", "NULL").await;
    exec(
        &h,
        format!(
            "INSERT INTO segments (id, chat_id, seq, started_at, reason, system_snapshot, tools_snapshot, hot_memory_ids)
               VALUES (1, 10, 1, {t}, 'initial', x'', x'', x'');
             INSERT INTO events (chat_id, segment_id, seq, kind, payload, turn_id, created_at)
               VALUES (10, 1, 1, 'message', x'', 'turn-1', {t});"
        ),
    )
    .await;
    let engine = Recording::default();
    let cron = Cron::new(h.kernel.clone(), engine.clone());

    let tick = cron.tick().await.expect("tick");
    assert_eq!(
        (tick.fired.len(), tick.skipped),
        (0, vec![String::from("daily")])
    );
    cron.settle().await;
    assert!(engine.seen.lock().expect("seen").is_empty());
    let (finished, ok, output, due) = single_run(&h).await;
    assert_eq!((finished, ok, due), (Some(t), Some(false), Some(t)));
    assert_eq!(output.as_deref(), Some(cron::SKIPPED));
    assert_eq!(job_next(&h).await, Some(t + hour()));
    assert_eq!(h.counter("cron.skipped").await, 1);
}

#[tokio::test]
async fn downtime_collapses_into_one_fire_and_keeps_the_phase() {
    let h = common::start().await;
    let t = t0(&h);
    seed_job(
        &h,
        Some(t - 5 * hour() - 7 * SEC),
        Some(hour()),
        "NULL",
        "NULL",
    )
    .await;
    let cron = Cron::new(h.kernel.clone(), Recording::default());

    let tick = cron.tick().await.expect("tick");
    assert_eq!(tick.fired.len(), 1);
    cron.settle().await;
    assert_eq!(job_next(&h).await, Some(t + hour() - 7 * SEC), "phase kept");
    let runs: i64 = h
        .query(|c| Ok(c.query_row("SELECT COUNT(*) FROM cron_runs", [], |r| r.get(0))?))
        .await;
    assert_eq!(runs, 1);
    assert_eq!(single_run(&h).await.3, Some(t - 5 * hour() - 7 * SEC));
    assert_eq!(h.counter("cron.collapsed").await, 1);
}

#[tokio::test]
async fn fire_pending_shift_and_keep() {
    let h = common::start().await;
    let t = t0(&h);
    seed_job(&h, Some(t + hour()), Some(2 * hour()), "'shift'", "NULL").await;
    let cron = Cron::new(h.kernel.clone(), Recording::default());
    assert_eq!(cron.tick().await.expect("tick").fired.len(), 1);
    cron.settle().await;
    assert_eq!(
        job_next(&h).await,
        Some(t + 2 * hour()),
        "shift: next slot from this run"
    );
    assert_eq!(
        single_run(&h).await.3,
        None,
        "a fire-only run has no planned slot"
    );

    let h = common::start().await;
    let t = t0(&h);
    seed_job(&h, Some(t + hour()), Some(2 * hour()), "'keep'", "NULL").await;
    let cron = Cron::new(h.kernel.clone(), Recording::default());
    assert_eq!(cron.tick().await.expect("tick").fired.len(), 1);
    cron.settle().await;
    assert_eq!(
        job_next(&h).await,
        Some(t + hour()),
        "keep: schedule untouched"
    );
    let pending: Option<String> = h
        .query(|c| Ok(c.query_row("SELECT fire_pending FROM cron_jobs", [], |r| r.get(0))?))
        .await;
    assert_eq!(pending, None, "the request is consumed");
}

#[tokio::test]
async fn expired_jobs_are_deleted_without_firing() {
    let h = common::start().await;
    let t = t0(&h);
    seed_job(&h, Some(t), Some(hour()), "NULL", "NULL").await;
    exec(&h, format!("UPDATE cron_jobs SET expires_at = {t}")).await;
    let engine = Recording::default();
    let cron = Cron::new(h.kernel.clone(), engine.clone());

    let tick = cron.tick().await.expect("tick");
    assert_eq!(
        (tick.expired, tick.fired.len()),
        (vec![String::from("daily")], 0)
    );
    cron.settle().await;
    let (jobs, deleted_at): (i64, Option<i64>) = h
        .query(|c| {
            Ok(c.query_row(
                "SELECT (SELECT COUNT(*) FROM cron_jobs), (SELECT deleted_at FROM chats WHERE id = 10)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .await;
    assert_eq!((jobs, deleted_at), (0, Some(t)));
    assert!(engine.seen.lock().expect("seen").is_empty());
}

#[tokio::test]
async fn one_shot_is_deleted_after_its_turn() {
    let h = common::start().await;
    let t = t0(&h);
    seed_job(&h, Some(t), None, "NULL", "NULL").await;
    let cron = Cron::new(h.kernel.clone(), Recording::default());
    assert_eq!(cron.tick().await.expect("tick").fired.len(), 1);
    assert_eq!(job_next(&h).await, None, "launched before the turn");
    cron.settle().await;
    let (jobs, deleted_at, runs): (i64, Option<i64>, i64) = h
        .query(|c| {
            Ok(c.query_row(
                "SELECT (SELECT COUNT(*) FROM cron_jobs), (SELECT deleted_at FROM chats WHERE id = 10),
                        (SELECT COUNT(*) FROM cron_runs WHERE job_id = 'job-a' AND ok = 1)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?)
        })
        .await;
    assert_eq!((jobs, deleted_at, runs), (0, Some(t), 1));
}

#[tokio::test]
async fn unreadable_whitelist_fails_without_a_turn() {
    let h = common::start().await;
    let t = t0(&h);
    seed_job(&h, Some(t), Some(hour()), "NULL", "'not json'").await;
    let engine = Recording::default();
    let cron = Cron::new(h.kernel.clone(), engine.clone());
    assert!(cron.tick().await.expect("tick").fired.is_empty());
    cron.settle().await;
    assert!(engine.seen.lock().expect("seen").is_empty());
    let (_, ok, output, _) = single_run(&h).await;
    assert_eq!(ok, Some(false));
    assert!(output.is_some_and(|o| o.starts_with("[cron failed] unreadable tool whitelist")));
    assert_eq!(
        job_next(&h).await,
        Some(t + hour()),
        "the slot still moves: no retry storm"
    );
}

#[tokio::test]
async fn a_failed_turn_is_recorded_and_counted() {
    let h = common::start().await;
    let t = t0(&h);
    seed_job(&h, Some(t), Some(hour()), "NULL", "'[1, 2]'").await;
    let engine = Recording::default();
    *engine.outcome.lock().expect("outcome") = Some(TurnOutcome::Failed {
        error: String::from("boom"),
    });
    let cron = Cron::new(h.kernel.clone(), engine.clone());
    assert_eq!(cron.tick().await.expect("tick").fired.len(), 1);
    cron.settle().await;
    let seen = engine.seen.lock().expect("seen").clone();
    assert_eq!(
        seen[0].tools.as_deref().map(|t| t.len()),
        Some(2),
        "ids are passed as the whitelist"
    );
    let (_, ok, output, _) = single_run(&h).await;
    assert_eq!(
        (ok, output.as_deref()),
        (Some(false), Some("[cron failed] boom"))
    );
    assert_eq!(h.counter("cron.failed").await, 1);
    assert_eq!(h.counter("cron.ok").await, 0);
}

#[tokio::test]
async fn recover_closes_runs_of_the_previous_process() {
    let h = common::start().await;
    let t = t0(&h);
    seed_job(&h, Some(t + hour()), Some(hour()), "NULL", "NULL").await;
    exec(
        &h,
        format!(
            "INSERT INTO cron_runs (run_id, job_id, started, due) VALUES ('r-open', 'job-a', {t}, {t}),
                                                                        ('r-done', 'job-a', {t}, {t});
             UPDATE cron_runs SET finished = {t}, ok = 1 WHERE run_id = 'r-done';"
        ),
    )
    .await;
    let cron = Cron::new(h.kernel.clone(), Recording::default());
    cron.recover().await.expect("recover");
    let (finished, ok, output, alerted): (Option<i64>, Option<bool>, Option<String>, bool) = h
        .query(|c| {
            Ok(c.query_row(
                "SELECT r.finished, r.ok, r.output, (SELECT alerted FROM chats WHERE id = 10)
                   FROM cron_runs r WHERE run_id = 'r-open'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?)
        })
        .await;
    assert_eq!(
        (finished, ok, output.as_deref(), alerted),
        (Some(t), Some(false), Some(cron::INTERRUPTED), true)
    );
    let done: Option<String> = h
        .query(|c| {
            Ok(c.query_row(
                "SELECT output FROM cron_runs WHERE run_id = 'r-done'",
                [],
                |r| r.get(0),
            )?)
        })
        .await;
    assert_eq!(done, None, "finished runs are untouched");
}

#[tokio::test]
async fn the_lag_gauge_reads_the_last_planned_run() {
    let h = common::start().await;
    let t = t0(&h);
    seed_job(&h, Some(t - 30 * SEC), Some(hour()), "NULL", "NULL").await;
    let cron = Cron::new(h.kernel.clone(), Recording::default());
    assert_eq!(cron.tick().await.expect("tick").fired.len(), 1);
    cron.settle().await;

    let today = h.clock.get().date().expect("date");
    let health::Ok { gauges, .. } = h
        .client_session()
        .await
        .stats()
        .health(today, today)
        .await
        .expect("health");
    assert_eq!(gauges.cron_fire_lag.len(), 1);
    assert_eq!(gauges.cron_fire_lag[0].job, "daily");
    assert_eq!(gauges.cron_fire_lag[0].lag.0, Duration::from_secs(30));
    assert!(gauges.next_gc_at.is_some(), "the gc slot is a gauge too");
}

/// Все `output` запусков по времени старта.
async fn outputs(h: &Harness) -> Vec<Option<String>> {
    h.query(|c| {
        let mut stmt = c.prepare("SELECT output FROM cron_runs ORDER BY started, run_id")?;
        let rows = stmt
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    })
    .await
}

#[tokio::test]
async fn a_turn_longer_than_the_interval_fires_once_per_slot() {
    let h = common::start().await;
    let t = t0(&h);
    seed_job(&h, Some(t), Some(hour()), "NULL", "NULL").await;
    let engine = Recording::default();
    let gate = Arc::new(Notify::new());
    *engine.gate.lock().expect("gate") = Some(gate.clone());
    let cron = Cron::new(h.kernel.clone(), engine.clone());
    let mut clock = h.clock.clone();

    assert_eq!(cron.tick().await.expect("tick").fired.len(), 1);
    clock.advance(Duration::from_secs(3600));
    let tick = cron.tick().await.expect("tick");
    assert_eq!(
        (tick.fired.len(), tick.skipped),
        (0, vec![String::from("daily")]),
        "the first turn is still running"
    );
    assert_eq!(
        job_next(&h).await,
        Some(t + 2 * hour()),
        "the schedule moves anyway"
    );

    *engine.gate.lock().expect("gate") = None;
    gate.notify_one();
    cron.settle().await;
    clock.advance(Duration::from_secs(3600));
    assert_eq!(
        cron.tick().await.expect("tick").fired.len(),
        1,
        "free again"
    );
    cron.settle().await;
    assert_eq!(engine.seen.lock().expect("seen").len(), 2);
    assert_eq!(
        outputs(&h).await,
        [
            Some(String::from("done")),
            Some(String::from(cron::SKIPPED)),
            Some(String::from("done"))
        ]
    );
}

#[tokio::test]
async fn a_fire_request_survives_a_restart() {
    let h = common::start().await;
    let t = t0(&h);
    // `cron_fire{shift: false}` записан, тик до рестарта не успел.
    seed_job(&h, Some(t + hour()), Some(hour()), "'keep'", "NULL").await;
    let Harness { kernel, dir, .. } = h;
    drop(kernel);
    let cfg = common::config(dir.path());
    let h = common::start_in(dir, cfg).await;

    let cron = Cron::new(h.kernel.clone(), Recording::default());
    cron.recover().await.expect("recover");
    assert_eq!(cron.tick().await.expect("tick").fired.len(), 1);
    cron.settle().await;
    assert!(
        cron.tick().await.expect("tick").fired.is_empty(),
        "exactly one fire"
    );
    assert_eq!(
        job_next(&h).await,
        Some(t + hour()),
        "keep leaves the schedule alone"
    );
}

#[tokio::test]
async fn nothing_fires_before_the_default_model_or_the_provider_wait() {
    let h = common::start_with(|cfg| {
        cfg.llm.default_model = eva_protocol::types::ModelId::from("not-served");
    })
    .await;
    let t = t0(&h);
    seed_job(&h, Some(t), Some(hour()), "NULL", "NULL").await;
    let cron = Cron::new(h.kernel.clone(), Recording::default());

    assert!(cron.tick().await.expect("tick").fired.is_empty());
    assert_eq!(
        job_next(&h).await,
        Some(t),
        "the schedule waits for inference"
    );
    let mut clock = h.clock.clone();
    clock.advance(*h.kernel.shared().cfg().llm.provider_wait);
    assert_eq!(
        cron.tick().await.expect("tick").fired.len(),
        1,
        "after provider_wait the turn fails loudly instead"
    );
}
