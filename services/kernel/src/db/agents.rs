//! Таблицы `agents`, `agent_messages`, `chat_plan`, `pending` (`07 §2`, `§4`, `§5`, `10 §2.7`).

use eva::{data, time::Timestamp};
use rusqlite::{Connection, Row};
use serde_json::Value;

use eva_protocol::types::{ChatId, ToolId, agent::Status};

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

impl Agents<'_> {
    /// Подагенты тимлида `parent` (или все) в порядке spawn.
    pub fn list(&self, parent: Option<ChatId>) -> DbResult<Vec<AgentRow>> {
        let mut stmt = self.0.prepare(
            "SELECT a.id, a.parent_chat_id, a.name, a.task, a.tools, a.report_schema, a.master_turn,
                    a.status, a.started_at, a.finished_at,
                    (SELECT COUNT(*) FROM agent_messages m
                      WHERE m.agent_id = a.id AND m.direction = 'to_parent' AND m.read_at IS NULL) AS unread
               FROM agents a
              WHERE ?1 IS NULL OR a.parent_chat_id = ?1
              ORDER BY a.started_at, a.id",
        )?;
        let raw = stmt
            .query_map([parent.map(ChatId::get)], |row| {
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
