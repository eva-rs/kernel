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

/// Границы выборки расходов; календарные даты включены, точные моменты — `[from, to)`.
#[data]
#[serde(deny_unknown_fields)]
pub enum SpendWindow {
    /// Полные календарные сутки в зоне запроса, включая обе даты.
    Dates { from: Date, to: Date },
    /// Точные моменты времени: `from` включён, `to` исключён.
    Instants {
        #[serde(deserialize_with = "deserialize_instant")]
        from: Timestamp,
        #[serde(deserialize_with = "deserialize_instant")]
        to: Timestamp,
    },
}

// Парсер дробной части Timestamp в framework разворачивает цифры; wire-формат сохраняем.
fn deserialize_instant<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Timestamp, D::Error> {
    use eva::time::SecsTime;
    use serde::{Deserialize as _, de::Error as _};

    if !deserializer.is_human_readable() {
        return Timestamp::deserialize(deserializer);
    }
    let value = String::deserialize(deserializer)?;
    let invalid = || D::Error::custom("expected DD.MM.YYYY HH:MM:SS.NNNNNNNNN in UTC");
    if value.len() != 29 || !value.is_ascii() {
        return Err(invalid());
    }
    let bytes = value.as_bytes();
    if bytes[10] != b' ' || bytes[19] != b'.' || !bytes[20..].iter().all(u8::is_ascii_digit) {
        return Err(invalid());
    }
    let date: Date = value[..10].parse().map_err(|_| invalid())?;
    // Проверка канонической даты также отвергает переполнение года при разборе framework.
    if date.to_str() != value[..10] {
        return Err(invalid());
    }
    let time: SecsTime = value[11..19].parse().map_err(|_| invalid())?;
    let fraction: u32 = value[20..].parse().map_err(|_| invalid())?;
    date.to_secs()
        .checked_add(time.to_secs())
        .and_then(|seconds| seconds.checked_mul(1_000_000_000))
        .and_then(|nanos| nanos.checked_add(u64::from(fraction)))
        .and_then(std::num::NonZeroU64::new)
        .map(Timestamp::from_nanos)
        .ok_or_else(invalid)
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
    /// Оценённые расходы по валютам; пусто — оценённых строк нет.
    pub keep_alive_cost: Vec<Money>,
    /// Оценка по текущему прайс-листу и `llm.currency`, только для строк `pricebook`.
    pub estimated_cost_saved: Option<Money>,
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
    /// Отдельный ряд на валюту; `None` — неоценённые строки.
    pub currency: Option<String>,
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
    /// По валютам; токены ниже включают также неоценённые строки.
    pub cost: Vec<Money>,
    pub prompt_total: u64,
    pub output: u64,
}

#[data]
pub struct TurnSpend {
    pub turn_id: TurnId,
    /// По валютам; токены ниже включают также неоценённые строки.
    pub cost: Vec<Money>,
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
    pub kind: module::ModuleKind,
    pub state: module::State,
    pub since: Timestamp,
    pub capabilities: Vec<String>,
    /// Только у записи реестра сервисов.
    pub dial: Option<module::Dial>,
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
