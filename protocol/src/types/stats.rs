//! Строки статистики.

use eva::{
    data,
    time::{Date, HumanDuration, Timestamp},
};

use crate::types::{ChatId, FolderId, ModelId, ModuleId, SurfaceId, ToolId, TurnId, module};

#[data(copy, ord)]
#[derive(Default)]
pub enum Scope {
    #[default]
    Chat,
    Folder,
    Surface,
    Global,
}

/// Объект разреза; обязан совпадать по виду со `Scope`.
#[data]
pub enum Target {
    Chat(ChatId),
    Folder(FolderId),
    Surface(SurfaceId),
}

#[data(copy, ord)]
#[derive(Default)]
pub enum GroupBy {
    #[default]
    Model,
    Source,
    Op,
    Chat,
    Day,
}

#[data]
pub struct ToolUsageRow {
    pub tool_id: ToolId,
    /// Текущее каноничное имя; `None` у удалённого тула.
    pub name: Option<String>,
    pub calls: u64,
    pub turns_with_call: u64,
    pub loads: u64,
    pub errors: u64,
    pub ends_turn: u64,
}

#[data]
pub struct SpendRow {
    /// Значение группы `GroupBy`.
    pub key: String,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cache_write_long: u64,
    pub output: u64,
    pub reasoning: u64,
    /// `None` — не оценено.
    pub cost: Option<f64>,
    pub currency: Option<String>,
}

#[data]
pub struct CacheStats {
    pub hit_ratio: f64,
    pub write_ratio: f64,
    pub cold_starts: u64,
    pub breaks: Vec<BreakCount>,
    pub context_edits: u64,
    pub keep_alive: u64,
    pub keep_alive_cost: Option<f64>,
    /// Только по строкам с ценой из прайс-бука.
    pub cost_saved: Option<f64>,
}

#[data(copy, ord, display(name))]
pub enum BreakReason {
    ModelChange,
    ToolsChange,
    SegmentReset,
    TtlExpired,
    ContextEdit,
    ThinkingChange,
    EffortChange,
    FirstRequest,
    Unknown,
}

#[data]
pub struct BreakCount {
    pub reason: BreakReason,
    pub count: u64,
}

#[data]
pub struct SeriesPoint {
    pub day: Date,
    pub key: String,
    pub cost: Option<f64>,
    pub prompt_total: u64,
    pub output: u64,
}

#[data]
pub struct Money {
    pub cost: f64,
    pub currency: String,
}

#[data]
pub struct ModelSpend {
    pub model: ModelId,
    pub cost: Option<Money>,
    pub prompt_total: u64,
    pub output: u64,
}

#[data]
pub struct TurnSpend {
    pub turn_id: TurnId,
    pub cost: Option<Money>,
    pub prompt_total: u64,
    pub cache_read: u64,
}

#[data]
pub struct Counter {
    pub metric: String,
    pub key: String,
    pub count: u64,
}

/// Живые величины: читаются из состояния компонентов в момент запроса.
#[data]
pub struct Gauges {
    pub modules: Vec<ModuleGauge>,
    pub turns_in_flight: u32,
    pub agents_running: u32,
    pub pending_open: u32,
    pub db_bytes: u64,
    pub uptime: HumanDuration,
    pub last_sleep_at: Option<Timestamp>,
    pub next_gc_at: Option<Timestamp>,
    pub gc_duration_last: Option<HumanDuration>,
    pub index_pending: u64,
    pub index_lag: Option<HumanDuration>,
    pub memory_hot_tokens: u32,
    pub memory_embed_pending: u64,
    pub cron_fire_lag: Vec<CronLag>,
    pub latency: Vec<Latency>,
}

#[data]
pub struct ModuleGauge {
    pub module: ModuleId,
    pub kind: module::TokenKind,
    pub state: module::State,
    pub since: Timestamp,
    pub capabilities: Vec<String>,
}

#[data]
pub struct CronLag {
    pub job: String,
    pub lag: HumanDuration,
}

#[data]
pub struct Latency {
    /// `tools.call` или `inference:<model>`.
    pub method: String,
    pub p50: HumanDuration,
    pub p95: HumanDuration,
}

#[data]
pub struct RateLimit {
    pub model: ModelId,
    pub requests_remaining: Option<u64>,
    pub requests_limit: Option<u64>,
    pub tokens_remaining: Option<u64>,
    pub tokens_limit: Option<u64>,
    pub reset_at: Option<Timestamp>,
    pub seen_at: Timestamp,
}
