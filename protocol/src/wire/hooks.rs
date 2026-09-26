//! Хуки (`06 §7`): синхронный `request hooks.<point>` с входом точки,
//! ответ — `Decision` с патчем поля, которое точка разрешает менять.

use eva::data;
use serde_json::Value;

use crate::{
    types::{ToolId, content::Block, turn::TurnCtx},
    wire::tools::ToolResult,
};

#[data(copy, ord, display(name))]
#[derive(Hash)]
#[serde(remote = "HookPoint")]
pub enum HookPoint {
    /// После резолва актора, до рендера.
    BeforeTurn,
    /// После Exposure-гейта, до исполнения.
    BeforeTool,
    /// До скраба секретов, усечения и записи в лог.
    AfterTool,
    /// Перед выдачей поверхности.
    BeforeSend,
    /// Точка новее этого пира: ядро её не зовёт.
    #[serde(other)]
    Unknown,
}

tolerant!(HookPoint);

impl HookPoint {
    /// RPC-метод точки.
    pub const fn method(self) -> &'static str {
        match self {
            Self::BeforeTurn => "hooks.before_turn",
            Self::BeforeTool => "hooks.before_tool",
            Self::AfterTool => "hooks.after_tool",
            Self::BeforeSend => "hooks.before_send",
            Self::Unknown => "hooks.unknown",
        }
    }
}

/// Вход точки и то, что её `Decision::Patch` заменяет.
pub trait Hook {
    const POINT: HookPoint;
    type Patch;
}

#[data]
pub struct BeforeTurn {
    pub ctx: TurnCtx,
    pub content: Vec<Block>,
}

impl Hook for BeforeTurn {
    const POINT: HookPoint = HookPoint::BeforeTurn;
    /// `content`.
    type Patch = Vec<Block>;
}

#[data]
pub struct BeforeTool {
    pub ctx: TurnCtx,
    /// `None` у `extra_tools` тура.
    pub tool_id: Option<ToolId>,
    pub canonical: String,
    pub input: Value,
}

impl Hook for BeforeTool {
    const POINT: HookPoint = HookPoint::BeforeTool;
    /// `input`.
    type Patch = Value;
}

#[data]
pub struct AfterTool {
    pub ctx: TurnCtx,
    pub tool_id: Option<ToolId>,
    pub canonical: String,
    pub input: Value,
    pub result: ToolResult,
}

impl Hook for AfterTool {
    const POINT: HookPoint = HookPoint::AfterTool;
    /// `result`.
    type Patch = ToolResult;
}

#[data]
pub struct BeforeSend {
    pub ctx: TurnCtx,
    pub text: String,
    /// Вложения ответа: блоки `image`/`file`.
    #[serde(default)]
    pub attachments: Vec<Block>,
}

impl Hook for BeforeSend {
    const POINT: HookPoint = HookPoint::BeforeSend;
    /// `text`.
    type Patch = String;
}

/// Ответ хука; патч передаётся следующему хуку цепочки.
#[data]
#[serde(remote = "Decision")]
pub enum Decision<P> {
    Continue,
    Cancel {
        reason: String,
    },
    Patch {
        payload: P,
    },
    /// Решение новее ядра: как отказ хука, по `fail_mode`.
    #[serde(other)]
    Unknown,
}

tolerant!(Decision<P>);
