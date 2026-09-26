//! Нормализованный запрос инференса и его поток — общий для трейта `Provider`
//! и запасной двери `provider.*`.

use eva::{
    data,
    time::{HumanDuration, Timestamp},
};
use serde_json::Value;

use crate::types::{Effort, ModelId, Thinking};

#[data]
pub struct GenerateRequest {
    pub model: ModelId,
    pub system: Vec<SystemSection>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub breakpoints: Vec<Position>,
    #[serde(default)]
    pub cache: CacheMode,
    pub effort: Option<Effort>,
    pub thinking: Option<Thinking>,
    /// 0 у `count_tokens` и keep-alive.
    #[serde(default)]
    pub max_tokens: u32,
    pub format: Option<OutputFormat>,
    #[serde(default)]
    pub purpose: Purpose,
}

#[data]
pub struct SystemSection {
    pub name: String,
    pub text: String,
}

#[data]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(default)]
    pub deferred: bool,
}

#[data]
pub enum Message {
    User {
        blocks: Vec<Block>,
    },
    Assistant {
        blocks: Vec<Block>,
    },
    System {
        kind: SystemKind,
        blocks: Vec<Block>,
        clear_at: Option<ClearAt>,
        output_config: Option<OutputConfig>,
        tool_changes: Option<ToolChanges>,
    },
}

/// Событие лога, отрендеренное system-сообщением.
#[data(copy, ord)]
pub enum SystemKind {
    NoteUpdated,
    MemoryUpdated,
    ToolsAdded,
    Reminder,
    EffortChanged,
    SystemText,
}

#[data(copy, ord)]
pub enum ClearAt {
    NextUserMessage,
}

#[data(copy)]
pub struct OutputConfig {
    pub effort: Effort,
}

#[data]
pub struct ToolChanges {
    pub added: Vec<String>,
}

#[data]
pub enum Block {
    Text {
        text: String,
    },
    Image {
        media_type: String,
        data: Vec<u8>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<ResultBlock>,
        #[serde(default)]
        is_error: bool,
    },
    /// Непрозрачен: реплеится байт-в-байт в своём assistant-сообщении.
    Thinking {
        signature: Vec<u8>,
        model: ModelId,
        text: Option<String>,
    },
    RedactedThinking {
        data: Vec<u8>,
        model: ModelId,
    },
}

#[data]
pub enum ResultBlock {
    Text { text: String },
    Image { media_type: String, data: Vec<u8> },
}

/// Позиция breakpoint'а в нормализованном запросе; провайдер переводит в свои индексы.
#[data(copy, ord)]
pub enum Position {
    /// Конец `system`.
    System,
    Message {
        index: u32,
    },
}

#[data(copy, ord)]
#[derive(Default)]
pub enum CacheMode {
    #[default]
    Off,
    Short,
    Long,
}

#[data]
pub struct OutputFormat {
    pub schema: Value,
    #[serde(default)]
    pub strict: bool,
}

#[data(copy, ord)]
#[derive(Default)]
pub enum Purpose {
    #[default]
    Turn,
    Compaction,
    Title,
    KeepAlive,
    /// `inference.complete` модуля.
    Complete,
}

/// Элемент потока `generate`.
#[data]
pub enum GenItem {
    TextDelta {
        text: String,
    },
    /// Пустой при summarized thinking.
    ThinkingDelta {
        text: String,
    },
    /// Закрывает текущий thinking.
    ThinkingBlock {
        signature: Vec<u8>,
        model: ModelId,
    },
    RedactedThinking {
        data: Vec<u8>,
        model: ModelId,
    },
    /// JSON аргументов склеен провайдером: частичного ядро не видит.
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    Image {
        media_type: String,
        data: Vec<u8>,
    },
    Usage(Usage),
    /// Последний элемент перед концом потока.
    Stop(Stop),
}

#[data]
#[derive(Default)]
#[serde(default)]
pub struct Usage {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write_short: u64,
    pub cache_write_long: u64,
    pub output: u64,
    pub reasoning: u64,
    pub context_edits: ContextEdits,
    pub cost: Option<f64>,
    pub rate_limits: Option<RateLimits>,
}

#[data(copy)]
#[derive(Default)]
#[serde(default)]
pub struct ContextEdits {
    pub applied: u32,
    pub cleared_input_tokens: u32,
}

#[data(copy)]
#[derive(Default)]
#[serde(default)]
pub struct RateLimits {
    pub requests_remaining: Option<u64>,
    pub tokens_remaining: Option<u64>,
    pub reset_at: Option<Timestamp>,
}

#[data]
pub struct Stop {
    pub reason: StopReason,
    pub details: Option<String>,
}

/// `refusal` — не ошибка, а исход шага.
#[data(copy, ord, display(name))]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    Other,
}

#[data(error)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub http_status: Option<u16>,
    /// Ответ вендора как есть — для лога, не для классификации.
    pub raw: String,
    /// Подсказка вендора для `transient`/`quota`; у in-process провайдера нет кадра `RpcError`, где её нести.
    pub retry_after: Option<HumanDuration>,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.raw)
    }
}

#[data(copy, ord, display(name))]
pub enum ProviderErrorKind {
    Transient,
    Auth,
    NotFound,
    Quota,
    /// Вход + `max_tokens` больше окна.
    Overflow,
    ContentFilter,
    Invalid,
    Other,
}

#[data]
pub struct EmbedRequest {
    pub model: ModelId,
    pub inputs: Vec<String>,
}

#[data]
pub struct EmbedResponse {
    pub vectors: Vec<Vec<f32>>,
    #[serde(default)]
    pub usage: Usage,
}
