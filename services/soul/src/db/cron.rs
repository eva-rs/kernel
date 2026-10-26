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
    sql::{Level, Text, Ts},
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
    pub due: Option<Timestamp>,
}

/// Запрос `cron_fire`, ждущий тика (`08 §1.4`).
#[data(copy, ord)]
pub enum FirePending {
    /// Расписание не трогается.
    Keep,
    /// Следующий слот — от этого запуска.
    Shift,
}

/// Джоба на тике (`08 §1.2`): всё, что решает сдвиг и строит тур.
#[data]
pub struct DueJob {
    pub id: CronJobId,
    pub name: String,
    pub task: String,
    pub interval: Option<Duration>,
    pub next_fire_at: Option<Timestamp>,
    pub fire_pending: Option<FirePending>,
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
    /// JSON `[tool_id]` как в колонке; разбирает компонент — нечитаемый whitelist не стреляет.
    pub tools: Option<String>,
    pub chat: ChatId,
    /// В чате идёт тур: срабатывание пропускается.
    pub live_turn: bool,
}

#[data]
pub struct NewRun {
    pub run: RunId,
    pub job: CronJobId,
    pub started: Timestamp,
    /// Плановый слот; `None` — запуск по `cron_fire`.
    pub due: Option<Timestamp>,
}

/// Джоба, удалённая по `expires_at`: чат помечается, хозяин уведомляется.
#[data]
pub struct Expired {
    pub name: String,
    pub chat: ChatId,
}

/// Незавершённый запуск прошлого процесса (`08 §1.6`).
#[data]
pub struct Interrupted {
    pub run: RunId,
    /// `None` — джоба уже удалена.
    pub job: Option<String>,
    pub chat: Option<ChatId>,
}

/// Открытый тур чата: события с `turn_id` в открытом сегменте без `turn_ended` (`01 §1.1`).
const LIVE_TURN: &str = "EXISTS (SELECT 1 FROM segments g JOIN events e ON e.segment_id = g.id
          WHERE g.chat_id = j.chat_id AND g.ended_at IS NULL AND e.turn_id IS NOT NULL
            AND NOT EXISTS (SELECT 1 FROM events x WHERE x.turn_id = e.turn_id AND x.kind = 'turn_ended'))";

pub struct Cron<'c>(pub &'c Connection);

impl Cron<'_> {
    /// Создание получает уже разрешённый whitelist.
    pub fn insert_job(&self, job: &JobRow, tools: Option<&[ToolId]>) -> DbResult<()> {
        let tools = tools
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| DbError::corrupt(format_args!("cron whitelist: {e}")))?;
        self.0.execute(
            "INSERT INTO cron_jobs (id, name, task, interval, next_fire_at, expires_at, model, effort, tools, chat_id, created_at)
             VALUES (:id, :name, :task, :interval, :next, :expires, :model, :effort, :tools, :chat, :created)",
            named_params! {
                ":id": job.id.as_str(), ":name": job.name, ":task": job.task,
                ":interval": job.interval.map(|i| i.0.as_nanos() as i64),
                ":next": job.next_fire_at.map(Ts), ":expires": job.expires_at.map(Ts),
                ":model": job.model.as_ref().map(ModelId::as_str), ":effort": job.effort.map(Level),
                ":tools": tools, ":chat": job.chat.get(), ":created": Ts(job.created_at),
            },
        )?;
        Ok(())
    }

    /// Обновление никогда не меняет расписание или уже запрошенное срабатывание.
    pub fn update_job(&self, job: &JobRow, tools: Option<Option<&[ToolId]>>) -> DbResult<()> {
        self.0.execute(
            "UPDATE cron_jobs SET name = :name, task = :task, interval = :interval,
                    expires_at = :expires, model = :model, effort = :effort WHERE id = :id",
            named_params! {
                ":id": job.id.as_str(), ":name": job.name, ":task": job.task,
                ":interval": job.interval.map(|i| i.0.as_nanos() as i64),
                ":expires": job.expires_at.map(Ts),
                ":model": job.model.as_ref().map(ModelId::as_str), ":effort": job.effort.map(Level),
            },
        )?;
        if let Some(tools) = tools {
            let tools = tools
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| DbError::corrupt(format_args!("cron whitelist: {e}")))?;
            self.0.execute(
                "UPDATE cron_jobs SET tools = ?2 WHERE id = ?1",
                (job.id.as_str(), tools),
            )?;
        }
        Ok(())
    }

    pub fn job(&self, id: &CronJobId) -> DbResult<Option<JobRow>> {
        Ok(self
            .0
            .query_row(
                "SELECT id, name, task, interval, next_fire_at, expires_at, model, effort, tools,
                    chat_id, runs_total, spent_total, created_at FROM cron_jobs WHERE id = ?1",
                [id.as_str()],
                job_from_row,
            )
            .optional()?)
    }

    pub fn name_taken(&self, name: &str, except: &CronJobId) -> DbResult<bool> {
        Ok(self.0.query_row(
            "SELECT EXISTS(SELECT 1 FROM cron_jobs WHERE name = ?1 AND id != ?2)",
            (name, except.as_str()),
            |row| row.get(0),
        )?)
    }

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
        let ids = parse_tools(job, json)?;
        ids.into_iter()
            .map(|id| {
                let name = crate::db::tools::Tools(self.0).canonical_name(id)?;
                Ok(name.unwrap_or_else(|| id.to_string()))
            })
            .collect()
    }

    /// Джобы, которым пора: слот наступил либо запрошен `cron_fire`.
    pub fn due(&self, now: Timestamp) -> DbResult<Vec<DueJob>> {
        let mut stmt = self.0.prepare(&format!(
            "SELECT j.id, j.name, j.task, j.interval, j.next_fire_at, j.fire_pending, j.model, j.effort,
                    j.tools, j.chat_id, {LIVE_TURN} AS live_turn
               FROM cron_jobs j
              WHERE j.next_fire_at <= ?1 OR j.fire_pending IS NOT NULL
              ORDER BY j.next_fire_at, j.id"
        ))?;
        let rows = stmt
            .query_map([Ts(now)], |row| {
                Ok(DueJob {
                    id: CronJobId(row.get("id")?),
                    name: row.get("name")?,
                    task: row.get("task")?,
                    interval: interval(row)?.map(|i| i.0),
                    next_fire_at: row.get::<_, Option<Ts>>("next_fire_at")?.map(|t| t.0),
                    fire_pending: row
                        .get::<_, Option<Text<FirePending>>>("fire_pending")?
                        .map(|t| t.0),
                    model: row.get::<_, Option<String>>("model")?.map(ModelId),
                    effort: row.get::<_, Option<Level>>("effort")?.map(|l| l.0),
                    tools: row.get("tools")?,
                    chat: ChatId(row.get("chat_id")?),
                    live_turn: row.get("live_turn")?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Слот перенесён до тура; запрос `cron_fire` снят.
    pub fn reschedule(&self, job: &CronJobId, next_fire_at: Option<Timestamp>) -> DbResult<()> {
        self.0.execute(
            "UPDATE cron_jobs SET next_fire_at = ?2, fire_pending = NULL WHERE id = ?1",
            (job.as_str(), next_fire_at.map(Ts)),
        )?;
        Ok(())
    }

    /// `cron_fire` (`08 §1.4`): запрос — строка БД, рестарт его не теряет. `false` — джобы нет.
    pub fn request_fire(&self, job: &CronJobId, pending: FirePending) -> DbResult<bool> {
        Ok(self.0.execute(
            "UPDATE cron_jobs SET fire_pending = CASE WHEN interval IS NULL THEN 'shift' ELSE ?2 END WHERE id = ?1",
            (job.as_str(), Text(pending)),
        )? > 0)
    }

    pub fn insert_run(&self, run: &NewRun) -> DbResult<()> {
        self.0.execute(
            "INSERT INTO cron_runs (run_id, job_id, started, due) VALUES (?1, ?2, ?3, ?4)",
            (
                run.run.as_str(),
                run.job.as_str(),
                Ts(run.started),
                run.due.map(Ts),
            ),
        )?;
        Ok(())
    }

    pub fn finish_run(
        &self,
        run: &RunId,
        finished: Timestamp,
        ok: bool,
        output: Option<&str>,
        cost: Option<f64>,
    ) -> DbResult<()> {
        self.0.execute(
            "UPDATE cron_runs SET finished = ?2, ok = ?3, output = ?4, cost = ?5 WHERE run_id = ?1",
            (run.as_str(), Ts(finished), ok, output, cost),
        )?;
        Ok(())
    }

    /// Итог запуска в джобе: `runs_total`, `spent_total` (`08 §1.3`).
    pub fn account(&self, job: &CronJobId, cost: f64) -> DbResult<()> {
        self.0.execute(
            "UPDATE cron_jobs SET runs_total = runs_total + 1, spent_total = spent_total + ?2 WHERE id = ?1",
            (job.as_str(), cost),
        )?;
        Ok(())
    }

    /// Джоба удалена; её чат — вызывающему на `deleted_at`. `None` — джобы уже нет.
    pub fn delete_job(&self, job: &CronJobId) -> DbResult<Option<ChatId>> {
        Ok(self
            .0
            .query_row(
                "DELETE FROM cron_jobs WHERE id = ?1 RETURNING chat_id",
                [job.as_str()],
                |row| row.get(0).map(ChatId),
            )
            .optional()?)
    }

    /// `expires_at <= now` → джоба удалена без срабатывания (`08 §1.2` п.1).
    pub fn delete_expired(&self, now: Timestamp) -> DbResult<Vec<Expired>> {
        let mut stmt = self
            .0
            .prepare("DELETE FROM cron_jobs WHERE expires_at <= ?1 RETURNING name, chat_id")?;
        let rows = stmt
            .query_map([Ts(now)], |row| {
                Ok(Expired {
                    name: row.get(0)?,
                    chat: ChatId(row.get(1)?),
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Старт после падения (`08 §1.6`): незавершённые запуски закрываются с `output`.
    pub fn interrupt_unfinished(&self, now: Timestamp, output: &str) -> DbResult<Vec<Interrupted>> {
        let mut stmt = self.0.prepare(
            "SELECT r.run_id, j.name, j.chat_id FROM cron_runs r
               LEFT JOIN cron_jobs j ON j.id = r.job_id
              WHERE r.finished IS NULL ORDER BY r.started, r.run_id",
        )?;
        let rows: Vec<Interrupted> = stmt
            .query_map([], |row| {
                Ok(Interrupted {
                    run: RunId(row.get(0)?),
                    job: row.get(1)?,
                    chat: row.get::<_, Option<i64>>(2)?.map(ChatId),
                })
            })?
            .collect::<Result<_, _>>()?;
        self.0.execute(
            "UPDATE cron_runs SET finished = ?1, ok = 0, output = ?2 WHERE finished IS NULL",
            (Ts(now), output),
        )?;
        Ok(rows)
    }

    /// `alerted` на чатах джоб, чьи запуски прервал рестарт (`08 §1.6`).
    pub fn alert_chats(&self, chats: &[ChatId]) -> DbResult<()> {
        let mut stmt = self
            .0
            .prepare("UPDATE chats SET alerted = 1 WHERE id = ?1")?;
        for chat in chats {
            stmt.execute([chat.get()])?;
        }
        Ok(())
    }

    /// Ретеншн `08 §3.2`: всё старше `keep` новейших.
    pub fn purge_runs(&self, keep: u32) -> DbResult<usize> {
        Ok(self.0.execute(
            "DELETE FROM cron_runs WHERE run_id NOT IN
               (SELECT run_id FROM cron_runs ORDER BY started DESC, run_id DESC LIMIT ?1)",
            [keep],
        )?)
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
            "SELECT run_id, job_id, started, finished, ok, output, cost, due FROM cron_runs
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

/// `cron_jobs.tools` → id; нечитаемая колонка — порча строки.
pub fn parse_tools(job: &CronJobId, json: &str) -> DbResult<Vec<ToolId>> {
    serde_json::from_str(json)
        .map_err(|e| DbError::corrupt(format_args!("cron_jobs.tools of {job}: {e}")))
}

fn interval(row: &Row<'_>) -> rusqlite::Result<Option<HumanDuration>> {
    row.get::<_, Option<i64>>("interval")?
        .map(|nanos| {
            u64::try_from(nanos)
                .map(|n| HumanDuration(Duration::from_nanos(n)))
                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(3, nanos))
        })
        .transpose()
}

fn job_from_row(row: &Row<'_>) -> rusqlite::Result<JobRow> {
    Ok(JobRow {
        id: CronJobId(row.get("id")?),
        name: row.get("name")?,
        task: row.get("task")?,
        interval: interval(row)?,
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
        due: row.get::<_, Option<Ts>>("due")?.map(|t| t.0),
    })
}

/// Неотрицательный INTEGER-счётчик: `u64` у rusqlite без фичи не читается.
fn count(row: &Row<'_>, column: &str) -> rusqlite::Result<u64> {
    let raw: i64 = row.get(column)?;
    u64::try_from(raw).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, raw))
}
