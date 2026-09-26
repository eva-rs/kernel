//! Подагент — чат с родителем.

use eva::{data, time::Timestamp};
use serde_json::Value;

use crate::types::{ChatId, ToolId};

#[data]
pub struct Agent {
    /// Чат подагента.
    pub chat: ChatId,
    pub parent: ChatId,
    pub name: String,
    pub task: String,
    pub tools: Vec<ToolId>,
    pub report_schema: Option<Value>,
    pub master_turn: bool,
    pub status: Status,
    /// Непрочитанные письма `to_parent`.
    pub unread: u32,
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
}

#[data(copy, ord)]
pub enum Status {
    Running,
    Idle,
    Done,
    Failed,
    Stopped,
}
