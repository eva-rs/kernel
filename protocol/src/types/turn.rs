//! Туры: ссылки на актора, проекции контекста тура и лента items.
//!
//! Лента (`TurnItem` и всё внутри) принимается терпимо: незнакомый вариант —
//! `Unknown`, соседние items разбираются дальше (`crate::tolerant`).

use eva::data;
use serde_json::Value;

use crate::types::{
    BlobId, ChatId, EventId, ModelId, PendingId, PersonId, RunId, SegmentId, SurfaceId, TurnId,
    tool::ToolContent,
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

/// Контекст тура на проводе (`tools.call`, хуки): soul сам считает флаги доверия.
#[data]
pub struct TurnCtx {
    pub turn_id: TurnId,
    pub chat: ChatId,
    pub actor: Actor,
    pub master_turn: bool,
    pub trusted_chat: bool,
    /// `¬master_turn ∨ ¬trusted_chat`.
    pub public: bool,
    pub surface: SurfaceId,
    /// Якорь поверхности из `chats.ingest`/`turns.start`, для soul непрозрачен.
    pub anchor: Option<Value>,
    /// Запуск крона.
    pub run_id: Option<RunId>,
}

/// Актор тура для логов и маркеров; гейты читают только `master_turn × trusted_chat`.
#[data]
pub struct Actor {
    /// `None` — тур без отправителя.
    pub person_id: Option<PersonId>,
    pub kind: ActorKind,
    pub name: Option<String>,
    pub handle: Option<String>,
}

impl Actor {
    /// Отправителя нет — никогда не хозяин.
    pub const fn unknown() -> Self {
        Self {
            person_id: None,
            kind: ActorKind::Unknown,
            name: None,
            handle: None,
        }
    }
}

#[data(copy, ord, display(name))]
#[serde(remote = "ActorKind")]
pub enum ActorKind {
    Master,
    Whitelisted,
    /// Сюда же — вид новее этого пира: незнакомый не хозяин.
    #[serde(other)]
    Unknown,
}

tolerant!(ActorKind);

/// Элемент `turns.attach`.
#[data]
pub struct TurnItem {
    /// `events.id` события-источника.
    pub seq: EventId,
    /// Порядок внутри события у items без своего события (`Plan`, `ClientOp`).
    #[serde(default)]
    pub sub: u32,
    /// Из истории или текущего состояния, не живое.
    #[serde(default)]
    pub replayed: bool,
    pub event: FeedEvent,
}

#[data]
#[serde(remote = "FeedEvent")]
pub enum FeedEvent {
    /// Оптимистичен: вызов начат.
    ItemStarted {
        item: Item,
    },
    ItemDelta {
        id: String,
        delta: ItemDelta,
    },
    /// Авторитетен: результат или ошибка; `replaces` — живой id, который
    /// этот исторический заменяет на месте.
    ItemCompleted {
        item: Item,
        replaces: Option<String>,
    },
    RequestResolved {
        pending: PendingId,
        outcome: PendingOutcome,
    },
    Usage(StepUsage),
    /// После `turn_ended`; `error` — текст исхода `failed`.
    TurnDone {
        turn: TurnId,
        outcome: TurnOutcome,
        error: Option<String>,
    },
    /// Событие новее этого пира.
    #[serde(other)]
    Unknown,
}

tolerant!(FeedEvent);

/// Исход тура — `turn_ended.outcome` лога.
#[data(copy, ord, display(name))]
#[serde(remote = "TurnOutcome")]
pub enum TurnOutcome {
    Completed,
    Cancelled,
    Failed,
    Interrupted,
    /// `before_turn` отклонил тур (`06 §7`).
    Blocked,
    /// Исход новее этого пира.
    #[serde(other)]
    Unknown,
}

tolerant!(TurnOutcome);

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
    /// Из лога `<event_id>:<n>`; живой `AgentMessage`/`Thinking` до записи —
    /// `<turn_id>:<step>:a` / `:t`; `ToolCall` — `call_id`; `ClientOp` — id
    /// pending; `Plan` — `plan:<chat_id>`.
    pub id: String,
    pub turn: Option<TurnId>,
    pub seq: EventId,
    pub status: ItemStatus,
    pub body: ItemBody,
}

#[data(copy, ord)]
#[serde(remote = "ItemStatus")]
pub enum ItemStatus {
    Running,
    Completed,
    Failed,
    Aborted,
    /// Статус новее этого пира.
    #[serde(other)]
    Unknown,
}

tolerant!(ItemStatus);

#[data]
#[serde(remote = "ItemBody")]
pub enum ItemBody {
    UserMessage {
        text: String,
        sender: Option<PersonId>,
        master: bool,
        /// Что прислала поверхность: клиент гасит им оптимистичный пузырь.
        external_id: Option<String>,
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
    File {
        blob: BlobId,
        url: String,
        name: String,
        media_type: String,
    },
    Reminder {
        text: String,
    },
    Plan {
        tasks: Vec<PlanTask>,
    },
    /// Item новее этого пира.
    #[serde(other)]
    Unknown,
}

tolerant!(ItemBody);

#[data]
#[serde(remote = "ItemDelta")]
pub enum ItemDelta {
    /// `AgentMessage`/`Thinking`.
    Text { text: String },
    Progress {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<Box<crate::types::text::LocalizedText>>,
    },
    /// План целиком.
    Plan { tasks: Vec<PlanTask> },
    /// Дельта новее этого пира.
    #[serde(other)]
    Unknown,
}

tolerant!(ItemDelta);

#[data]
pub struct ToolCallResult {
    pub output: Vec<ToolContent>,
    pub is_error: bool,
}

#[data(copy, ord)]
#[serde(remote = "PendingKind")]
pub enum PendingKind {
    Ask,
    Permission,
    /// Вид новее этого пира.
    #[serde(other)]
    Unknown,
}

tolerant!(PendingKind);

/// Кнопка pending-запроса.
#[data(copy, ord)]
#[serde(remote = "Decision")]
pub enum Decision {
    Pick,
    TextInstead,
    Allow,
    AllowSession,
    AllowAlways,
    Deny,
    /// Кнопка новее этого пира.
    #[serde(other)]
    Unknown,
}

tolerant!(Decision);

#[data(copy, ord)]
#[serde(remote = "PendingOutcome")]
pub enum PendingOutcome {
    Answered,
    TextInstead,
    /// `ask`, отвеченный после `ask_ttl`: ответ уходит в чат user-сообщением `[ask] late answer …`.
    LateAnswer,
    /// Просроченный `ask` закрыт следующей человеческой репликой.
    Superseded,
    Allowed,
    Denied,
    Expired,
    Cancelled,
    /// Исход новее этого пира.
    #[serde(other)]
    Unknown,
}

tolerant!(PendingOutcome);

#[data]
pub struct PlanTask {
    pub id: u32,
    pub phase: Option<String>,
    pub title: String,
    pub status: TaskStatus,
    pub outcome: Option<String>,
}

#[data(copy, ord)]
#[serde(remote = "TaskStatus")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Abandoned,
    /// Статус новее этого пира.
    #[serde(other)]
    Unknown,
}

tolerant!(TaskStatus);

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
