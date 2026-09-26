//! Туры: ссылки на актора, проекция контекста тура и лента items.

use eva::data;
use serde_json::Value;

use crate::types::{
    BlobId, ChatId, EventId, ModelId, PendingId, PersonId, SegmentId, TurnId, tool::ToolContent,
};

/// Отправитель на поверхности; surface берётся из чата или аргумента операции, не из ссылки.
#[data]
pub struct ActorRef {
    pub external_id: String,
    pub handle: Option<String>,
    pub name: Option<String>,
}

/// Судьба реплики после входа.
#[data]
pub enum TurnState {
    /// Silent либо актор не в whitelist.
    None,
    /// Реплика ушла в уже живой тур.
    Live(TurnId),
    Started(TurnId),
}

impl TurnState {
    pub fn turn(&self) -> Option<&TurnId> {
        match self {
            Self::None => None,
            Self::Live(id) | Self::Started(id) => Some(id),
        }
    }
}

#[data(copy, ord)]
pub enum Audience {
    MasterOnly,
    Mixed,
}

/// Взгляд тура: только сужает то, что вернёт операция; `None` у операции — взгляд хозяина.
#[data]
pub struct TurnView {
    pub chat: ChatId,
    /// `None` — неизвестный актор.
    pub actor: Option<PersonId>,
    pub master_turn: bool,
    pub trusted_chat: bool,
    pub audience: Audience,
}

impl TurnView {
    /// `¬master_turn ∨ ¬trusted_chat`.
    pub const fn public(&self) -> bool {
        !(self.master_turn && self.trusted_chat)
    }
}

/// Элемент `turns.attach`.
#[data]
pub struct TurnItem {
    /// `events.id` события-источника.
    pub seq: EventId,
    /// Порядок внутри события у items без своего события (`Plan`, `ClientOp`).
    #[serde(default)]
    pub sub: u32,
    /// Из буфера или истории, не живое.
    #[serde(default)]
    pub replayed: bool,
    pub event: FeedEvent,
}

#[data]
pub enum FeedEvent {
    /// Оптимистичен: вызов начат.
    ItemStarted {
        item: Item,
    },
    ItemDelta {
        id: String,
        delta: ItemDelta,
    },
    /// Авторитетен: результат или ошибка.
    ItemCompleted {
        item: Item,
    },
    RequestResolved {
        pending: PendingId,
        outcome: PendingOutcome,
    },
    Usage(StepUsage),
    /// `message_ids` заменяют live-id на исторические.
    TurnDone {
        turn: TurnId,
        message_ids: Vec<EventId>,
    },
    TurnError {
        turn: TurnId,
        message: String,
    },
}

#[data]
pub struct StepUsage {
    pub turn: TurnId,
    pub step: u32,
    pub model: ModelId,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub cost: Option<f64>,
}

#[data]
pub struct Item {
    /// Из истории `<message_id>:<n>`, живое `<turn_id>:<n>`, у `ToolCall` — `call_id`.
    pub id: String,
    pub turn: Option<TurnId>,
    pub seq: EventId,
    pub status: ItemStatus,
    pub body: ItemBody,
}

#[data(copy, ord)]
pub enum ItemStatus {
    Running,
    Completed,
    Failed,
    Aborted,
}

#[data]
pub enum ItemBody {
    UserMessage {
        text: String,
        sender: Option<PersonId>,
        master: bool,
    },
    AgentMessage {
        text: String,
        cost: Option<f64>,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        input: Value,
        /// Подагент, чей это вызов.
        agent: Option<String>,
        result: Option<ToolCallResult>,
        progress: Option<String>,
    },
    ClientOp {
        pending: PendingId,
        kind: PendingKind,
        payload: Value,
        decisions: Vec<Decision>,
        resolution: Option<PendingOutcome>,
    },
    Image {
        blob: BlobId,
        /// `eva-blob://<id>`: поверхность перехостит сама.
        url: String,
        name: String,
        media_type: String,
    },
    Compaction {
        summary_ref: SegmentId,
    },
    Reminder {
        text: String,
    },
    Plan {
        tasks: Vec<PlanTask>,
    },
}

#[data]
pub enum ItemDelta {
    /// `AgentMessage`/`Thinking`.
    Text {
        text: String,
    },
    Progress {
        text: String,
    },
    /// План целиком.
    Plan {
        tasks: Vec<PlanTask>,
    },
}

#[data]
pub struct ToolCallResult {
    pub output: Vec<ToolContent>,
    pub is_error: bool,
}

#[data(copy, ord)]
pub enum PendingKind {
    Ask,
    Permission,
}

/// Кнопка pending-запроса.
#[data(copy, ord)]
pub enum Decision {
    Pick,
    TextInstead,
    Allow,
    AllowSession,
    AllowAlways,
    Deny,
}

#[data(copy, ord)]
pub enum PendingOutcome {
    Answered,
    TextInstead,
    Allowed,
    Denied,
    Expired,
    Cancelled,
}

#[data]
pub struct PlanTask {
    pub id: u32,
    pub phase: Option<String>,
    pub title: String,
    pub status: TaskStatus,
    pub outcome: Option<String>,
}

#[data(copy, ord)]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Abandoned,
}

/// Ответ на pending: форма зависит от его вида.
#[data]
pub enum Answer {
    /// `ask`: по выбору на вопрос.
    Pick {
        picks: Vec<Pick>,
    },
    /// `ask`: текст вместо кнопок растворяет все вопросы вызова.
    Text {
        text: String,
    },
    Permission {
        decision: Permission,
    },
}

#[data(copy)]
pub struct Pick {
    pub question: u8,
    pub option: u8,
}

#[data(copy, ord)]
pub enum Permission {
    Allow,
    AllowSession,
    AllowAlways,
    Deny,
}
