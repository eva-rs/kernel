//! Таблицы `sleep_runs`, `sleep_phase_runs`, `gc_runs` (`08 §2`, `§3`, `10 §2.8`).

use eva::{data, time::Timestamp};
use rusqlite::{Connection, OptionalExtension as _, Row, named_params};

use eva_protocol::types::{ModelId, RunId, sleep::Phase};

use crate::db::{
    DbResult,
    sql::{Text, Ts},
};

#[data]
pub struct RunRow {
    pub run: RunId,
    pub started: Timestamp,
    pub finished: Option<Timestamp>,
    pub ok: Option<bool>,
    pub dry_run: bool,
    pub interrupted: bool,
    pub report: Option<String>,
}

#[data]
pub struct PhaseRow {
    pub run: RunId,
    pub phase: Phase,
    pub started: Timestamp,
    pub finished: Option<Timestamp>,
    pub model: Option<ModelId>,
    /// JSON `[PhaseOp]` как в колонке `ops_json`.
    pub ops_json: String,
    pub applied: u32,
    pub discarded: u32,
    pub report: Option<String>,
}

/// Последний завершённый прогон GC для gauge'а `gc_duration_last`.
#[data(copy)]
pub struct GcRun {
    pub started: Timestamp,
    pub finished: Timestamp,
}

pub struct Sleep<'c>(pub &'c Connection);

impl Sleep<'_> {
    pub fn exists(&self, run: &RunId) -> DbResult<bool> {
        Ok(self
            .0
            .query_row(
                "SELECT 1 FROM sleep_runs WHERE run_id = ?1",
                [run.as_str()],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Новейшие первыми, keyset по `(started, run_id)`; `after` обязан существовать.
    pub fn runs(&self, after: Option<&RunId>, limit: u16) -> DbResult<Vec<RunRow>> {
        let mut stmt = self.0.prepare(
            "SELECT run_id, started, finished, ok, dry_run, interrupted, report FROM sleep_runs
              WHERE :after IS NULL
                 OR (started, run_id) < (SELECT started, run_id FROM sleep_runs WHERE run_id = :after)
              ORDER BY started DESC, run_id DESC LIMIT :limit",
        )?;
        let rows = stmt
            .query_map(
                named_params! {
                    ":after": after.map(RunId::as_str),
                    ":limit": limit,
                },
                run_from_row,
            )?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Фазы ночи в порядке `memory → groups → warm`.
    pub fn phases(&self, run: &RunId) -> DbResult<Vec<PhaseRow>> {
        let mut stmt = self.0.prepare(
            "SELECT run_id, phase, started, finished, model, ops_json, applied, discarded, report
               FROM sleep_phase_runs WHERE run_id = ?1
              ORDER BY CASE phase WHEN 'memory' THEN 0 WHEN 'groups' THEN 1 WHEN 'warm' THEN 2 ELSE 3 END",
        )?;
        let rows = stmt
            .query_map([run.as_str()], |row| {
                Ok(PhaseRow {
                    run: RunId(row.get("run_id")?),
                    phase: row.get::<_, Text<Phase>>("phase")?.0,
                    started: row.get::<_, Ts>("started")?.0,
                    finished: row.get::<_, Option<Ts>>("finished")?.map(|t| t.0),
                    model: row.get::<_, Option<String>>("model")?.map(ModelId),
                    ops_json: row.get("ops_json")?,
                    applied: row.get("applied")?,
                    discarded: row.get("discarded")?,
                    report: row.get("report")?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Начало последней ночи (gauge `last_sleep_at`).
    pub fn last_started(&self) -> DbResult<Option<Timestamp>> {
        let ts: Option<Ts> =
            self.0
                .query_row("SELECT MAX(started) FROM sleep_runs", [], |row| row.get(0))?;
        Ok(ts.map(|t| t.0))
    }

    pub fn last_gc(&self) -> DbResult<Option<GcRun>> {
        Ok(self
            .0
            .query_row(
                "SELECT started, finished FROM gc_runs WHERE finished IS NOT NULL
                  ORDER BY started DESC, run_id DESC LIMIT 1",
                [],
                |row| {
                    Ok(GcRun {
                        started: row.get::<_, Ts>(0)?.0,
                        finished: row.get::<_, Ts>(1)?.0,
                    })
                },
            )
            .optional()?)
    }
}

fn run_from_row(row: &Row<'_>) -> rusqlite::Result<RunRow> {
    Ok(RunRow {
        run: RunId(row.get("run_id")?),
        started: row.get::<_, Ts>("started")?.0,
        finished: row.get::<_, Option<Ts>>("finished")?.map(|t| t.0),
        ok: row.get("ok")?,
        dry_run: row.get("dry_run")?,
        interrupted: row.get("interrupted")?,
        report: row.get("report")?,
    })
}
