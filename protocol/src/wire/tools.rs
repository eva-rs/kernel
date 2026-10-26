//! `tools.call` soul → модуль (`06 §4`, `02 §4.2`): запрос `ToolCall`,
//! поток `progress`*, затем один `result`.

use eva::data;
use serde_json::Value;

use crate::types::{ToolId, tool::ToolContent, turn::TurnCtx};

#[data]
pub struct ToolCall {
    /// `tool_use.id` модели: по нему SDK дедупит повтор после реконнекта.
    pub call_id: String,
    /// `None` у `extra_tools` тура.
    pub tool_id: Option<ToolId>,
    pub canonical: String,
    pub input: Value,
    pub ctx: TurnCtx,
}

/// Элемент потока `tools.call`; `stream_end{error}` — `Fatal`, тур падает.
#[data]
#[serde(remote = "ToolItem")]
pub enum ToolItem {
    /// Проходит скраб секретов до ленты items.
    Progress {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<Box<crate::types::text::LocalizedText>>,
    },
    /// Последний элемент.
    Result(ToolResult),
    /// Элемент новее этого пира.
    #[serde(other)]
    Unknown,
}

tolerant!(ToolItem);

/// `is_error: true` — данные для модели (`Respond`), тур продолжается.
#[data]
pub struct ToolResult {
    pub content: Vec<ToolContent>,
    #[serde(default)]
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(content: Vec<ToolContent>) -> Self {
        Self {
            content,
            is_error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContent::text(text)],
            is_error: true,
        }
    }
}
