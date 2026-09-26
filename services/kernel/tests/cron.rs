//! `cron.*` (`08 §1.7`): джобы с актуальными канонами whitelist, журнал
//! запусков keyset'ом по `(started, run_id)`, матрица `Policy`.

mod common;

use std::time::Duration;

use common::{Harness, Outcome, outcome};
use eva::time::Clock as _;
use eva_core::{CallStep as _, service::cron::Cron as _};
use eva_kernel::db::{Db as _, DbResult};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::{
        RunsLimit,
        cron::{list, runs},
    },
    types::{CronJobId, Effort, RunId},
};

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
               VALUES (1, 'kernel', 'fs.read', 'fs_read', '{{}}', x'00', {t}, {t}, NULL),
                      (2, 'kernel', 'old.tool', 'old_tool', '{{}}', x'01', {t}, {t}, {t});
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
    seed_jobs(&h, "[1, 2, 99]").await;

    let list::Ok { jobs } = h
        .client_session()
        .await
        .cron()
        .list()
        .call(list::Args {})
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
                String::from("fs_read"),
                String::from("old_tool"),
                String::from("tool99")
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
    let res = h
        .client_session()
        .await
        .cron()
        .list()
        .call(list::Args {})
        .await;
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
        .runs()
        .call(runs::Args {
            job: job.map(CronJobId::from),
            limit: RunsLimit::new(limit).expect("limit in range"),
            after: after.map(RunId::from),
        })
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
        let listed = outcome(&s.cron().list().call(list::Args {}).await);
        let runs = outcome(&s.cron().runs().call(runs::Args::default()).await);
        for (op, got) in [("list", listed), ("runs", runs)] {
            if got != want {
                failures.push(format!("cron.{op} as {who}: want {want:?}, got {got:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
