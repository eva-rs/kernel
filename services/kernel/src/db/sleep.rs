//! Таблицы `sleep_runs`, `sleep_phase_runs` (`08 §2`, `10 §2.8`), тишина перед
//! ночью и правки `memory`, которые делает только сон (`10 §6`).

use eva::{data, time::Timestamp};
use rusqlite::{Connection, OptionalExtension as _, Row, named_params};

use eva_protocol::types::{
    ChatId, MemoryId, ModelId, PersonId, RunId,
    memory::{Kind, Visibility},
    sleep::Phase,
};

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

/// Тишина перед ночью (`08 §2.1`).
#[data(copy)]
pub struct Quiet {
    /// Живые туры в чатах вне surface `system`.
    pub live_turns: u32,
    /// Последняя человеческая реплика: `role = user` не от `p0`.
    pub last_human_at: Option<Timestamp>,
}

/// Итог фазы для строки `sleep_phase_runs`.
#[data]
pub struct PhaseEnd<'a> {
    pub finished: Timestamp,
    pub ops_json: &'a str,
    pub applied: u32,
    pub discarded: u32,
    pub report: Option<&'a str>,
}

/// Победитель merge после слияния (`08 §2.3`).
#[data]
pub struct Merged {
    pub text: String,
    pub kind: Kind,
    pub visibility: Visibility,
    pub importance: f64,
    pub created_at: Timestamp,
    pub last_reinforced_at: Timestamp,
    pub access_count: f64,
    pub source_trusted_chat: Option<bool>,
    pub now: Timestamp,
}

/// Маркер фазы, оборванной падением ядра (`08 §2.6`).
pub const INTERRUPTED: &str = "[interrupted]";

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

    /// Начало последней ночи (gauge `last_sleep_at`, due `08 §2.1`).
    pub fn last_started(&self) -> DbResult<Option<Timestamp>> {
        let ts: Option<Ts> =
            self.0
                .query_row("SELECT MAX(started) FROM sleep_runs", [], |row| row.get(0))?;
        Ok(ts.map(|t| t.0))
    }

    /// `started` пишется до фаз: «когда спала» переживает падение.
    pub fn insert_run(&self, run: &RunId, started: Timestamp, dry_run: bool) -> DbResult<()> {
        self.0.execute(
            "INSERT INTO sleep_runs (run_id, started, dry_run) VALUES (?1, ?2, ?3)",
            (run.as_str(), Ts(started), dry_run),
        )?;
        Ok(())
    }

    pub fn finish_run(
        &self,
        run: &RunId,
        finished: Timestamp,
        ok: bool,
        interrupted: bool,
        report: Option<&str>,
    ) -> DbResult<()> {
        self.0.execute(
            "UPDATE sleep_runs SET finished = ?2, ok = ?3, interrupted = ?4, report = ?5 WHERE run_id = ?1",
            (run.as_str(), Ts(finished), ok, interrupted, report),
        )?;
        Ok(())
    }

    pub fn insert_phase(
        &self,
        run: &RunId,
        phase: Phase,
        started: Timestamp,
        model: Option<&ModelId>,
    ) -> DbResult<()> {
        self.0.execute(
            "INSERT INTO sleep_phase_runs (run_id, phase, started, model) VALUES (?1, ?2, ?3, ?4)",
            (
                run.as_str(),
                Text(phase),
                Ts(started),
                model.map(ModelId::as_str),
            ),
        )?;
        Ok(())
    }

    pub fn finish_phase(&self, run: &RunId, phase: Phase, end: &PhaseEnd<'_>) -> DbResult<()> {
        self.0.execute(
            "UPDATE sleep_phase_runs SET finished = :finished, ops_json = :ops, applied = :applied,
                    discarded = :discarded, report = :report
              WHERE run_id = :run AND phase = :phase",
            named_params! {
                ":run": run.as_str(),
                ":phase": Text(phase),
                ":finished": Ts(end.finished),
                ":ops": end.ops_json,
                ":applied": end.applied,
                ":discarded": end.discarded,
                ":report": end.report,
            },
        )?;
        Ok(())
    }

    /// Старт после падения (`08 §2.6`): незакрытые ночи — `interrupted`, их
    /// фазы без `finished` — `[interrupted]`. Возвращает прерванные ночи.
    pub fn recover(&self, now: Timestamp) -> DbResult<Vec<RunId>> {
        let mut stmt = self
            .0
            .prepare("SELECT run_id FROM sleep_runs WHERE finished IS NULL ORDER BY started")?;
        let runs: Vec<RunId> = stmt
            .query_map([], |row| row.get(0).map(RunId))?
            .collect::<Result<_, _>>()?;
        self.0.execute(
            "UPDATE sleep_phase_runs SET finished = ?1, report = ?2
              WHERE finished IS NULL
                AND run_id IN (SELECT run_id FROM sleep_runs WHERE finished IS NULL)",
            (Ts(now), INTERRUPTED),
        )?;
        self.0.execute(
            "UPDATE sleep_runs SET finished = ?1, ok = 0, interrupted = 1 WHERE finished IS NULL",
            [Ts(now)],
        )?;
        Ok(runs)
    }

    /// Живые туры вне `system` и последняя человеческая реплика (`08 §2.1`).
    pub fn quiet(&self) -> DbResult<Quiet> {
        let live_turns: u32 = self.0.query_row(
            "SELECT COUNT(*) FROM segments g
               JOIN chats c ON c.id = g.chat_id
               JOIN folders f ON f.id = c.folder_id
              WHERE g.ended_at IS NULL AND f.surface_id <> 'system'
                AND EXISTS (SELECT 1 FROM events e
                             WHERE e.segment_id = g.id AND e.turn_id IS NOT NULL
                               AND NOT EXISTS (SELECT 1 FROM events x
                                                WHERE x.turn_id = e.turn_id AND x.kind = 'turn_ended'))",
            [],
            |row| row.get(0),
        )?;
        let last_human_at: Option<Ts> = self.0.query_row(
            "SELECT MAX(e.created_at) FROM messages m JOIN events e ON e.id = m.event_id
              WHERE m.role = 'user' AND (m.sender_person_id IS NULL OR m.sender_person_id <> ?1)",
            [PersonId::system().as_str()],
            |row| row.get(0),
        )?;
        Ok(Quiet {
            live_turns,
            last_human_at: last_human_at.map(|t| t.0),
        })
    }

    /// Текущих записей памяти: порог `sleep.min_memories`.
    pub fn memory_count(&self) -> DbResult<u64> {
        let n: i64 = self.0.query_row(
            "SELECT COUNT(*) FROM memory WHERE valid_to IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Чат `sleep` не удаляется никем (`08 §2.2`).
    pub fn make_undeletable(&self, chat: ChatId) -> DbResult<()> {
        self.0.execute(
            "UPDATE chats SET undeletable = 1 WHERE id = ?1",
            [chat.get()],
        )?;
        Ok(())
    }

    /// Ретеншн `08 §3.2`: всё старше `keep` новейших, фазы — каскадом.
    pub fn purge_runs(&self, keep: u32) -> DbResult<usize> {
        Ok(self.0.execute(
            "DELETE FROM sleep_runs WHERE run_id NOT IN
               (SELECT run_id FROM sleep_runs ORDER BY started DESC, run_id DESC LIMIT ?1)",
            [keep],
        )?)
    }

    /// Слияние в победителя (`08 §2.3`); `false` — версия ушла от снапшота.
    pub fn merge_into(&self, winner: MemoryId, version: u32, m: &Merged) -> DbResult<bool> {
        let changed = self.0.execute(
            "UPDATE memory SET text = :text, kind = :kind, visibility = :visibility,
                    importance = :importance, created_at = :created, last_reinforced_at = :reinforced,
                    access_count = :access, source_trusted_chat = :trusted,
                    version = version + 1, updated_at = :now
              WHERE id = :id AND version = :version AND valid_to IS NULL",
            named_params! {
                ":id": winner.get(),
                ":version": version,
                ":text": m.text,
                ":kind": Text(m.kind),
                ":visibility": Text(m.visibility),
                ":importance": m.importance,
                ":created": Ts(m.created_at),
                ":reinforced": Ts(m.last_reinforced_at),
                ":access": m.access_count,
                ":trusted": m.source_trusted_chat,
                ":now": Ts(m.now),
            },
        )?;
        Ok(changed == 1)
    }

    /// `change_type`/`demote`: `kind` под проверкой версии; `false` — stale.
    pub fn set_kind(
        &self,
        id: MemoryId,
        version: u32,
        kind: Kind,
        now: Timestamp,
    ) -> DbResult<bool> {
        let changed = self.0.execute(
            "UPDATE memory SET kind = ?3, version = version + 1, updated_at = ?4
              WHERE id = ?1 AND version = ?2 AND valid_to IS NULL",
            (id.get(), version, Text(kind), Ts(now)),
        )?;
        Ok(changed == 1)
    }

    /// Осколок наследует `created_at` исходника (`08 §2.3`).
    pub fn inherit_created(&self, id: MemoryId, created_at: Timestamp) -> DbResult<()> {
        self.0.execute(
            "UPDATE memory SET created_at = ?2 WHERE id = ?1",
            (id.get(), Ts(created_at)),
        )?;
        Ok(())
    }

    pub fn link_split(&self, shard: MemoryId, source: MemoryId, now: Timestamp) -> DbResult<()> {
        self.0.execute(
            "INSERT OR IGNORE INTO memory_link (from_id, to_id, rel, created_at)
             VALUES (?1, ?2, 'split_from', ?3)",
            (shard.get(), source.get(), Ts(now)),
        )?;
        Ok(())
    }

    /// По концу фазы показанные и порождённые записи — `consolidated_at = now`.
    pub fn consolidate(&self, ids: &[MemoryId], now: Timestamp) -> DbResult<()> {
        let mut stmt = self
            .0
            .prepare("UPDATE memory SET consolidated_at = ?2 WHERE id = ?1 AND valid_to IS NULL")?;
        for id in ids {
            stmt.execute((id.get(), Ts(now)))?;
        }
        Ok(())
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
