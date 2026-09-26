//! Таблицы `cron_jobs`, `cron_runs` (`08 §1`, `10 §2.8`).

use std::time::Duration;

use eva::{
    data,
    time::{HumanDuration, Timestamp},
};
use rusqlite::{Connection, OptionalExtension as _, Row, named_params};

use eva_protocol::types::{ChatId, CronJobId, Effort, ModelId, RunId, ToolId};

use crate::db::{
    DbError, DbResult,
    sql::{Level, Ts},
};

#[data]
pub struct JobRow {
    pub id: CronJobId,
    pub name: String,
    pub task: String,
    pub interval: Option<HumanDuration>,
    pub next_fire_at: Option<Timestamp>,
    pub expires_at: Option<Timestamp>,
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
    /// Текущие каноничные имена whitelist; `None` — без ограничений (I8).
    pub tools: Option<Vec<String>>,
    pub chat: ChatId,
    pub runs_total: u64,
    pub spent_total: f64,
    pub created_at: Timestamp,
}

#[data]
pub struct RunRow {
    pub run: RunId,
    pub job: CronJobId,
    pub started: Timestamp,
    pub finished: Option<Timestamp>,
    pub ok: Option<bool>,
    pub output: Option<String>,
    pub cost: Option<f64>,
}

pub struct Cron<'c>(pub &'c Connection);

impl Cron<'_> {
    /// Все джобы по времени создания; whitelist — актуальными канонами.
    pub fn jobs(&self) -> DbResult<Vec<JobRow>> {
        let mut stmt = self.0.prepare(
            "SELECT id, name, task, interval, next_fire_at, expires_at, model, effort, tools,
                    chat_id, runs_total, spent_total, created_at
               FROM cron_jobs ORDER BY created_at, id",
        )?;
        let raw = stmt
            .query_map([], |row| {
                Ok((job_from_row(row)?, row.get::<_, Option<String>>("tools")?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        raw.into_iter()
            .map(|(job, tools)| {
                let tools = tools
                    .map(|json| self.canonical(&job.id, &json))
                    .transpose()?;
                Ok(JobRow { tools, ..job })
            })
            .collect()
    }

    /// `tools` хранит id (`08 §1.1`): имя берётся текущее, пропавший id — `tool<N>`.
    fn canonical(&self, job: &CronJobId, json: &str) -> DbResult<Vec<String>> {
        let ids: Vec<ToolId> = serde_json::from_str(json)
            .map_err(|e| DbError::corrupt(format_args!("cron_jobs.tools of {job}: {e}")))?;
        let mut stmt = self
            .0
            .prepare("SELECT canonical FROM tools WHERE tool_id = ?1")?;
        ids.into_iter()
            .map(|id| {
                let name: Option<String> =
                    stmt.query_row([id.get()], |row| row.get(0)).optional()?;
                Ok(name.unwrap_or_else(|| id.to_string()))
            })
            .collect()
    }

    pub fn run_exists(&self, run: &RunId) -> DbResult<bool> {
        Ok(self
            .0
            .query_row(
                "SELECT 1 FROM cron_runs WHERE run_id = ?1",
                [run.as_str()],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Новейшие первыми, keyset по `(started, run_id)`.
    pub fn runs(
        &self,
        job: Option<&CronJobId>,
        after: Option<&RunId>,
        limit: u16,
    ) -> DbResult<Vec<RunRow>> {
        let mut stmt = self.0.prepare(
            "SELECT run_id, job_id, started, finished, ok, output, cost FROM cron_runs
              WHERE (:job IS NULL OR job_id = :job)
                AND (:after IS NULL
                     OR (started, run_id) < (SELECT started, run_id FROM cron_runs WHERE run_id = :after))
              ORDER BY started DESC, run_id DESC LIMIT :limit",
        )?;
        let rows = stmt
            .query_map(
                named_params! {
                    ":job": job.map(CronJobId::as_str),
                    ":after": after.map(RunId::as_str),
                    ":limit": limit,
                },
                run_from_row,
            )?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }
}

fn job_from_row(row: &Row<'_>) -> rusqlite::Result<JobRow> {
    let interval = row
        .get::<_, Option<i64>>("interval")?
        .map(|nanos| {
            u64::try_from(nanos)
                .map(|n| HumanDuration(Duration::from_nanos(n)))
                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(3, nanos))
        })
        .transpose()?;

    Ok(JobRow {
        id: CronJobId(row.get("id")?),
        name: row.get("name")?,
        task: row.get("task")?,
        interval,
        next_fire_at: row.get::<_, Option<Ts>>("next_fire_at")?.map(|t| t.0),
        expires_at: row.get::<_, Option<Ts>>("expires_at")?.map(|t| t.0),
        model: row.get::<_, Option<String>>("model")?.map(ModelId),
        effort: row.get::<_, Option<Level>>("effort")?.map(|l| l.0),
        tools: None,
        chat: ChatId(row.get("chat_id")?),
        runs_total: count(row, "runs_total")?,
        spent_total: row.get("spent_total")?,
        created_at: row.get::<_, Ts>("created_at")?.0,
    })
}

fn run_from_row(row: &Row<'_>) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        run: RunId(row.get("run_id")?),
        job: CronJobId(row.get("job_id")?),
        started: row.get::<_, Ts>("started")?.0,
        finished: row.get::<_, Option<Ts>>("finished")?.map(|t| t.0),
        ok: row.get("ok")?,
        output: row.get("output")?,
        cost: row.get("cost")?,
    })
}

/// Неотрицательный INTEGER-счётчик: `u64` у rusqlite без фичи не читается.
fn count(row: &Row<'_>, column: &str) -> rusqlite::Result<u64> {
    let raw: i64 = row.get(column)?;
    u64::try_from(raw).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, raw))
}
