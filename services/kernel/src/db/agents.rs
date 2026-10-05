//! Таблицы `agents`, `agent_messages`, `chat_plan`, `pending` (`07 §2`, `§4`, `§5`, `10 §2.7`).

use eva::{data, time::Timestamp};
use rusqlite::{Connection, Row};
use serde_json::Value;

use eva_protocol::types::{
    ChatId, PendingId, ToolId, TurnId,
    agent::Status,
    turn::{PendingKind, PendingOutcome, TaskStatus},
};

use crate::db::{
    DbError, DbResult,
    sql::{Text, Ts},
};

#[data]
pub struct AgentRow {
    pub chat: ChatId,
    pub parent: ChatId,
    pub name: String,
    pub task: String,
    pub tools: Vec<ToolId>,
    pub report_schema: Option<Value>,
    pub master_turn: bool,
    pub status: Status,
    pub unread: u32,
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
}

pub struct Agents<'c>(pub &'c Connection);

#[data]
pub struct Letter {
    pub id: i64,
    pub agent: ChatId,
    pub body: String,
}

impl Agents<'_> {
    pub fn get(&self, chat: ChatId) -> DbResult<Option<AgentRow>> {
        Ok(self.query(None, Some(chat))?.into_iter().next())
    }

    pub fn insert(&self, agent: &AgentRow) -> DbResult<()> {
        self.0.execute(
            "INSERT INTO agents (id, parent_chat_id, name, task, tools, report_schema, master_turn, status, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            (agent.chat.0, agent.parent.0, &agent.name, &agent.task,
             serde_json::to_string(&agent.tools).map_err(DbError::corrupt)?,
             agent.report_schema.as_ref().map(serde_json::to_string).transpose().map_err(DbError::corrupt)?,
             agent.master_turn, Text(agent.status), Ts(agent.started_at)),
        )?;
        Ok(())
    }

    /// Завершённый подагент не воскресает от запоздалого callback тура.
    pub fn set_status(&self, chat: ChatId, status: Status, now: Timestamp) -> DbResult<bool> {
        let finished =
            matches!(status, Status::Done | Status::Failed | Status::Stopped).then_some(Ts(now));
        Ok(self.0.execute(
            "UPDATE agents SET status = ?2, finished_at = ?3 WHERE id = ?1 AND status IN ('running', 'idle')",
            (chat.0, Text(status), finished),
        )? > 0)
    }

    pub fn letter(
        &self,
        chat: ChatId,
        to_parent: bool,
        body: &str,
        now: Timestamp,
    ) -> DbResult<i64> {
        Ok(self.0.query_row(
            "INSERT INTO agent_messages (agent_id, direction, body, created_at)
             VALUES (?1, ?2, ?3, ?4) RETURNING id",
            (
                chat.0,
                if to_parent { "to_parent" } else { "to_agent" },
                body,
                Ts(now),
            ),
            |r| r.get(0),
        )?)
    }

    pub fn unread(&self, chat: ChatId, to_parent: bool) -> DbResult<Vec<Letter>> {
        let mut stmt = self.0.prepare(
            "SELECT m.id, m.agent_id, m.body FROM agent_messages m JOIN agents a ON a.id = m.agent_id
             WHERE m.read_at IS NULL AND m.direction = ?2
               AND ((?3 AND a.parent_chat_id = ?1) OR (NOT ?3 AND m.agent_id = ?1))
             ORDER BY m.id",
        )?;
        Ok(stmt
            .query_map(
                (
                    chat.0,
                    if to_parent { "to_parent" } else { "to_agent" },
                    to_parent,
                ),
                |r| {
                    Ok(Letter {
                        id: r.get(0)?,
                        agent: ChatId(r.get(1)?),
                        body: r.get(2)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn read_letters(&self, ids: &[i64], now: Timestamp) -> DbResult<()> {
        let mut stmt = self
            .0
            .prepare("UPDATE agent_messages SET read_at = ?2 WHERE id = ?1 AND read_at IS NULL")?;
        for id in ids {
            stmt.execute((id, Ts(now)))?;
        }
        Ok(())
    }

    /// Подагенты тимлида `parent` (или все) в порядке spawn.
    pub fn list(&self, parent: Option<ChatId>) -> DbResult<Vec<AgentRow>> {
        self.query(parent, None)
    }

    fn query(&self, parent: Option<ChatId>, chat: Option<ChatId>) -> DbResult<Vec<AgentRow>> {
        let mut stmt = self.0.prepare(
            "SELECT a.id, a.parent_chat_id, a.name, a.task, a.tools, a.report_schema, a.master_turn,
                    a.status, a.started_at, a.finished_at,
                    (SELECT COUNT(*) FROM agent_messages m
                      WHERE m.agent_id = a.id AND m.direction = 'to_parent' AND m.read_at IS NULL) AS unread
               FROM agents a
              WHERE (?1 IS NULL OR a.parent_chat_id = ?1) AND (?2 IS NULL OR a.id = ?2)
              ORDER BY a.started_at, a.id",
        )?;
        let raw = stmt
            .query_map((parent.map(ChatId::get), chat.map(ChatId::get)), |row| {
                Ok((
                    from_row(row)?,
                    row.get::<_, String>("tools")?,
                    row.get::<_, Option<String>>("report_schema")?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        raw.into_iter()
            .map(|(agent, tools, schema)| {
                let corrupt = |what: &str, e: serde_json::Error| {
                    DbError::corrupt(format_args!("agents.{what} of {}: {e}", agent.chat))
                };
                let tools = serde_json::from_str(&tools).map_err(|e| corrupt("tools", e))?;
                let report_schema = schema
                    .map(|s| serde_json::from_str(&s))
                    .transpose()
                    .map_err(|e| corrupt("report_schema", e))?;
                Ok(AgentRow {
                    tools,
                    report_schema,
                    ..agent
                })
            })
            .collect()
    }

    /// Gauge `agents_running` (`09 §4`).
    pub fn running(&self) -> DbResult<u32> {
        Ok(self.0.query_row(
            "SELECT COUNT(*) FROM agents WHERE status = 'running'",
            [],
            |row| row.get(0),
        )?)
    }

    /// Gauge `pending_open` (`09 §4`).
    pub fn pending_open(&self) -> DbResult<u32> {
        Ok(self.0.query_row(
            "SELECT COUNT(*) FROM pending WHERE resolved_at IS NULL",
            [],
            |row| row.get(0),
        )?)
    }

    /// Подагенты чата в `running`/`idle` (`07 §1`, `Need::Team`).
    pub fn live_children(&self, chat: ChatId) -> DbResult<u32> {
        Ok(self.0.query_row(
            "SELECT COUNT(*) FROM agents WHERE parent_chat_id = ?1 AND status IN ('running', 'idle')",
            [chat.0],
            |row| row.get(0),
        )?)
    }

    // ---- pending (`07 §4.2`) ----

    pub fn insert_pending(&self, p: &PendingRow) -> DbResult<()> {
        let payload = serde_json::to_string(&p.payload).map_err(DbError::corrupt)?;
        self.0.execute(
            "INSERT INTO pending (id, chat_id, turn_id, call_id, kind, payload, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (
                p.id.as_str(),
                p.chat.0,
                p.turn.as_str(),
                &p.call_id,
                Text(p.kind),
                payload,
                Ts(p.created_at),
                Ts(p.expires_at),
            ),
        )?;
        Ok(())
    }

    pub fn pending(&self, id: &PendingId) -> DbResult<Option<PendingRow>> {
        let mut stmt = self.0.prepare(&format!("{PENDING} WHERE id = ?1"))?;
        let row = stmt
            .query_map([id.as_str()], pending_from_row)?
            .next()
            .transpose()?;
        row.transpose()
    }

    /// Открытые pending чата по времени создания.
    pub fn open_pending(&self, chat: ChatId) -> DbResult<Vec<PendingRow>> {
        let mut stmt = self.0.prepare(&format!(
            "{PENDING} WHERE chat_id = ?1 AND resolved_at IS NULL ORDER BY created_at, id"
        ))?;
        let rows = stmt
            .query_map([chat.0], pending_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().collect()
    }

    /// `false` — уже разрешён: второй ответ — `conflict`.
    pub fn resolve_pending(
        &self,
        id: &PendingId,
        outcome: PendingOutcome,
        at: Timestamp,
    ) -> DbResult<bool> {
        let n = self.0.execute(
            "UPDATE pending SET outcome = ?2, resolved_at = ?3 WHERE id = ?1 AND resolved_at IS NULL",
            (id.as_str(), Text(outcome), Ts(at)),
        )?;
        Ok(n > 0)
    }

    /// Открытые pending тура (или все при `None`) → `cancelled`; возвращает закрытые.
    pub fn cancel_pending(
        &self,
        turn: Option<&TurnId>,
        at: Timestamp,
    ) -> DbResult<Vec<PendingRow>> {
        let mut stmt = self.0.prepare(&format!(
            "{PENDING} WHERE resolved_at IS NULL AND (?1 IS NULL OR turn_id = ?1) ORDER BY created_at, id"
        ))?;
        let rows = stmt
            .query_map([turn.map(TurnId::as_str)], pending_from_row)?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .collect::<DbResult<Vec<_>>>()?;
        self.0.execute(
            "UPDATE pending SET outcome = 'cancelled', resolved_at = ?2
              WHERE resolved_at IS NULL AND (?1 IS NULL OR turn_id = ?1)",
            (turn.map(TurnId::as_str), Ts(at)),
        )?;
        Ok(rows)
    }

    // ---- chat_plan (`07 §5`) ----

    pub fn plan(&self, chat: ChatId) -> DbResult<Vec<PlanRow>> {
        let mut stmt = self.0.prepare(
            "SELECT task_id, position, phase, title, status, outcome, updated_at FROM chat_plan
              WHERE chat_id = ?1 ORDER BY position, task_id",
        )?;
        let rows = stmt
            .query_map([chat.0], |row| {
                Ok(PlanRow {
                    task_id: row.get(0)?,
                    position: row.get(1)?,
                    phase: row.get(2)?,
                    title: row.get(3)?,
                    status: row.get::<_, Text<TaskStatus>>(4)?.0,
                    outcome: row.get(5)?,
                    updated_at: row.get::<_, Ts>(6)?.0,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// План целиком: операция считается на копии, пишется DELETE+INSERT.
    pub fn replace_plan(&self, chat: ChatId, rows: &[PlanRow]) -> DbResult<()> {
        self.0
            .execute("DELETE FROM chat_plan WHERE chat_id = ?1", [chat.0])?;
        let mut stmt = self.0.prepare(
            "INSERT INTO chat_plan (chat_id, task_id, position, phase, title, status, outcome, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for r in rows {
            stmt.execute((
                chat.0,
                r.task_id,
                r.position,
                &r.phase,
                &r.title,
                Text(r.status),
                &r.outcome,
                Ts(r.updated_at),
            ))?;
        }
        Ok(())
    }

    /// Вход в master-тур: закрытые задачи уходят (`07 §5` п.6).
    pub fn purge_settled(&self, chat: ChatId) -> DbResult<u64> {
        let n = self.0.execute(
            "DELETE FROM chat_plan WHERE chat_id = ?1 AND status IN ('completed', 'abandoned')",
            [chat.0],
        )?;
        Ok(n as u64)
    }

    /// Конец тура: план без открытых задач удаляется целиком.
    pub fn delete_plan_if_closed(&self, chat: ChatId) -> DbResult<bool> {
        let n = self.0.execute(
            "DELETE FROM chat_plan WHERE chat_id = ?1 AND NOT EXISTS (
               SELECT 1 FROM chat_plan WHERE chat_id = ?1 AND status IN ('pending', 'in_progress'))",
            [chat.0],
        )?;
        Ok(n > 0)
    }
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<AgentRow> {
    Ok(AgentRow {
        chat: ChatId(row.get("id")?),
        parent: ChatId(row.get("parent_chat_id")?),
        name: row.get("name")?,
        task: row.get("task")?,
        tools: Vec::new(),
        report_schema: None,
        master_turn: row.get("master_turn")?,
        status: row.get::<_, Text<Status>>("status")?.0,
        unread: row.get("unread")?,
        started_at: row.get::<_, Ts>("started_at")?.0,
        finished_at: row.get::<_, Option<Ts>>("finished_at")?.map(|t| t.0),
    })
}

/// Строка `pending` (`07 §4.2`).
#[data]
pub struct PendingRow {
    pub id: PendingId,
    pub chat: ChatId,
    pub turn: TurnId,
    pub call_id: String,
    pub kind: PendingKind,
    pub payload: Value,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    pub resolved_at: Option<Timestamp>,
    pub outcome: Option<PendingOutcome>,
}

/// Строка `chat_plan` (`07 §5`).
#[data]
pub struct PlanRow {
    pub task_id: u32,
    pub position: u32,
    pub phase: Option<String>,
    pub title: String,
    pub status: TaskStatus,
    pub outcome: Option<String>,
    pub updated_at: Timestamp,
}

const PENDING: &str = "SELECT id, chat_id, turn_id, call_id, kind, payload, created_at, expires_at,
       resolved_at, outcome FROM pending";

fn pending_from_row(row: &Row<'_>) -> rusqlite::Result<DbResult<PendingRow>> {
    let id = PendingId(row.get(0)?);
    let payload: String = row.get(5)?;
    let base = PendingRow {
        chat: ChatId(row.get(1)?),
        turn: TurnId(row.get(2)?),
        call_id: row.get(3)?,
        kind: row.get::<_, Text<PendingKind>>(4)?.0,
        payload: Value::Null,
        created_at: row.get::<_, Ts>(6)?.0,
        expires_at: row.get::<_, Ts>(7)?.0,
        resolved_at: row.get::<_, Option<Ts>>(8)?.map(|t| t.0),
        outcome: row.get::<_, Option<Text<PendingOutcome>>>(9)?.map(|o| o.0),
        id,
    };
    Ok(serde_json::from_str(&payload)
        .map(|payload| PendingRow {
            payload,
            ..base.clone()
        })
        .map_err(|e| DbError::corrupt(format_args!("pending.payload of {}: {e}", base.id))))
}
