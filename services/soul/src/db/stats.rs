//! Таблицы `tool_stats`, `chat_stats`, `spend`, `health_stats` (`09`, `10 §2.9`)
//! и чтения чужих таблиц для живых gauge'ей (`09 §4`).

use std::{collections::BTreeMap, time::Duration};

use eva::{
    data,
    time::{Date, Time, Timestamp},
};
use jiff::{
    Zoned,
    civil::Weekday,
    tz::{Offset, TimeZone},
};
use rusqlite::{
    Connection, OptionalExtension as _, Row, functions::FunctionFlags, named_params, types::Value,
};

use eva_protocol::types::{
    ChatId, FolderId, ModelId, ModuleId, RunId, SurfaceId, ToolId, TurnId,
    module::State,
    stats::{BreakReason, GroupBy, SpendWindow},
};

use crate::db::{
    DbError, DbResult,
    modules::{Cost, CostSource},
    sql::{Text, Ts},
};

/// `at − by`, не раньше `Timestamp::MIN`.
pub fn ago(at: Timestamp, by: Duration) -> Timestamp {
    let nanos = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
    std::num::NonZeroU64::new(at.as_nanos().get().saturating_sub(nanos))
        .map_or(Timestamp::MIN, Timestamp::from_nanos)
}

/// `at + by`, не позже `Timestamp::MAX`.
pub fn later(at: Timestamp, by: Duration) -> Timestamp {
    let nanos = u64::try_from(by.as_nanos()).unwrap_or(u64::MAX);
    Timestamp::from_nanos(at.as_nanos().saturating_add(nanos))
}

/// `to − from`; часы мира могут отстать от записанного — тогда ноль.
pub fn elapsed(from: Timestamp, to: Timestamp) -> Duration {
    Duration::from_nanos(to.as_nanos().get().saturating_sub(from.as_nanos().get()))
}

/// Реестр метрик здоровья `health_stats` (`09 §5`, I11): `(вариант, имя в БД)`.
/// Список закрыт; фича добавляет свою строку сюда, иначе её счётчика нет.
macro_rules! metrics {
    ($($variant:ident = $name:literal,)*) => {
        /// Счётчик здоровья: имя в БД — `as_str`.
        #[data(copy, ord)]
        #[derive(Hash)]
        pub enum Metric {
            $($variant,)*
        }

        impl Metric {
            pub const ALL: &'static [Self] = &[$(Self::$variant,)*];

            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }
        }
    };
}

metrics! {
    AgentsReported = "agents.reported",
    AgentsSilent = "agents.silent",
    AgentsFailed = "agents.failed",
    AgentsStopped = "agents.stopped",
    AgentsWokeLeader = "agents.woke_leader",
    CronOk = "cron.ok",
    CronFailed = "cron.failed",
    CronCollapsed = "cron.collapsed",
    CronSkipped = "cron.skipped",
    SleepOpsApplied = "sleep.ops_applied",
    SleepOpsDiscarded = "sleep.ops_discarded",
    SleepRuns = "sleep.runs",
    SleepSkipped = "sleep.skipped",
    SleepInterrupted = "sleep.interrupted",
    ModulesConnected = "modules.connected",
    ModulesDisconnected = "modules.disconnected",
    ModulesReconnects = "modules.reconnects",
    ModulesDialFailed = "modules.dial_failed",
    ModulesRpcErrors = "modules.rpc_errors",
    ModulesForbidden = "modules.forbidden",
    HooksTimeout = "hooks.timeout",
    HooksError = "hooks.error",
    PendingResolved = "pending.resolved",
    PendingExpired = "pending.expired",
    PendingCancelled = "pending.cancelled",
    TurnsCompleted = "turns.completed",
    TurnsCancelled = "turns.cancelled",
    TurnsFailed = "turns.failed",
    TurnsStepCap = "turns.step_cap",
    TurnsInterrupted = "turns.interrupted",
    Compactions = "compactions",
    CompactionFailed = "compaction_failed",
    NotesModeTransition = "notes.mode_transition",
    NotesDarkCompactions = "notes.dark_compactions",
    NotesRejected = "notes.rejected",
    MemoryRecallHits = "memory.recall_hits",
    MemoryRecallEmpty = "memory.recall_empty",
    MemoryRecallWithheld = "memory.recall_withheld",
    MemoryEmbedTimeouts = "memory.embed_timeouts",
    MemoryHotDropped = "memory.hot_dropped",
    MemoryWrites = "memory.writes",
    MemoryDedupMerged = "memory.dedup_merged",
    MemoryForgotten = "memory.forgotten",
    ToolsDenied = "tools.denied",
    ToolsCollisions = "tools.collisions",
    ToolsSpecRejected = "tools.spec_rejected",
    ToolsCoreMissing = "tools.core_missing",
    ToolsCatalogOmitted = "tools.catalog_omitted",
    ToolsWarmOverBudget = "tools.warm_over_budget",
    ChatsIngest = "chats.ingest",
    ChatsAudienceOpened = "chats.audience_opened",
    ChatsAutoRestored = "chats.auto_restored",
    ChatsTrustMoves = "chats.trust_moves",
    ChatsTitle = "chats.title",
    PersonsCreated = "persons.created",
    NotifySent = "notify.sent",
    SecretsRedactions = "secrets.redactions",
    InferenceRetries = "inference.retries",
    InferenceFallbacks = "inference.fallbacks",
    InferencePartialRetry = "inference.partial_retry",
    InferenceErrors = "inference.errors",
    InferenceRefusals = "inference.refusals",
    InferenceRateLimited = "inference.rate_limited",
    InferenceUsageMissing = "inference.usage_missing",
    CacheColdStart = "cache.cold_start",
    CacheKeepAlive = "cache.keep_alive",
    CacheBreak = "cache.break",
    SpendUnpriced = "spend.unpriced",
    GcPurged = "gc.purged",
}

/// Кто запустил инференс (`09 §2`).
#[data(copy, ord)]
pub enum Source {
    Surface,
    Cron,
    Sleep,
    Agent,
    System,
    Module,
}

/// Что за вызов инференса (`09 §2`).
#[data(copy, ord)]
pub enum Op {
    Step,
    Compaction,
    Title,
    Embed,
    KeepAlive,
    Vet,
    Other,
}

/// Строка `spend` на один usage-чанк провайдера (`09 §2`).
#[data]
pub struct SpendRow {
    pub ts: Timestamp,
    pub model: ModelId,
    pub source: Source,
    pub op: Op,
    /// Обязан быть `Some` ⇔ `source = Module`.
    pub module: Option<ModuleId>,
    pub chat: Option<ChatId>,
    pub turn: Option<TurnId>,
    pub run: Option<RunId>,
    pub input: u64,
    pub cache_read: u64,
    /// Запись любого TTL; `cache_write_long` — её часть.
    pub cache_write: u64,
    pub cache_write_long: u64,
    pub output: u64,
    pub reasoning: u64,
    pub context_edits_applied: u32,
    pub cache_break_reason: Option<BreakReason>,
    /// `None` — не оценено (`cost_source = none`).
    pub cost: Option<Cost>,
}

/// Вклад одного тура в бакет `tool_stats(day, chat, tool)` (`09 §1.3`).
#[data(copy)]
#[derive(Default)]
pub struct ToolDelta {
    pub calls: u32,
    pub errors: u32,
    pub ends_turn: u32,
    pub loads: u32,
}

/// Вклад одного тура в `chat_stats(day, chat)` (`09 §1.2`).
#[data(copy)]
#[derive(Default)]
pub struct ChatDelta {
    pub turns: u32,
    pub steps: u32,
    pub user_messages: u32,
    pub assistant_messages: u32,
    pub cancelled: u32,
}

/// Отставание последнего планового запуска джобы от слота (`08 §4`).
#[data]
pub struct CronLag {
    pub job: String,
    pub lag: Duration,
}

/// Сколько строк статистики унёс GC (`09 §8`).
#[data(copy)]
#[derive(Default)]
pub struct StatsPurged {
    pub tool_stats: usize,
    pub chat_stats: usize,
    pub health_stats: usize,
    pub spend: usize,
}

/// Разрез агрегата (`09 §1.4`): положение чата на момент запроса.
#[data]
pub enum ScopeFilter {
    Global,
    Chat(ChatId),
    Folder(FolderId),
    Surface(SurfaceId),
    /// Папка чата тура: `target` опущен при `view`.
    FolderOf(ChatId),
    /// Surface чата тура.
    SurfaceOf(ChatId),
}

impl ScopeFilter {
    /// Условие на колонку чата `col`; всегда ссылается на `:target`.
    fn sql(&self, col: &str) -> String {
        match self {
            Self::Global => String::from(":target IS NULL"),
            Self::Chat(_) => format!("{col} = :target"),
            Self::Folder(_) => format!("{col} IN (SELECT id FROM chats WHERE folder_id = :target)"),
            Self::Surface(_) => format!(
                "{col} IN (SELECT c.id FROM chats c JOIN folders f ON f.id = c.folder_id
                            WHERE f.surface_id = :target)"
            ),
            Self::FolderOf(_) => format!(
                "{col} IN (SELECT id FROM chats
                            WHERE folder_id = (SELECT folder_id FROM chats WHERE id = :target))"
            ),
            Self::SurfaceOf(_) => format!(
                "{col} IN (SELECT c.id FROM chats c JOIN folders f ON f.id = c.folder_id
                            WHERE f.surface_id = (SELECT f2.surface_id FROM chats c2
                                                    JOIN folders f2 ON f2.id = c2.folder_id
                                                   WHERE c2.id = :target))"
            ),
        }
    }

    fn target(&self) -> Value {
        match self {
            Self::Global => Value::Null,
            Self::Chat(id) | Self::FolderOf(id) | Self::SurfaceOf(id) => Value::Integer(id.get()),
            Self::Folder(id) => Value::Integer(id.get()),
            Self::Surface(id) => Value::Text(id.0.clone()),
        }
    }
}

/// Период запроса: календарные сутки включительно.
#[data(copy)]
pub struct Period {
    pub from: Date,
    pub to: Date,
}

impl Period {
    fn first_day(self) -> String {
        day_str(self.from)
    }

    fn last_day(self) -> String {
        day_str(self.to)
    }
}

/// Проверенные границы `spend.ts` в наносекундах: `[from, to)`.
#[data(copy)]
pub struct SpendPeriod {
    pub from: i64,
    pub to: i64,
}

impl SpendPeriod {
    pub fn resolve(window: SpendWindow, zone: &DayZone) -> Result<Self, String> {
        match window {
            SpendWindow::Instants { from, to } => {
                if from >= to {
                    return Err(String::from("instant window requires from < to"));
                }
                let nanos = |at: Timestamp| {
                    i64::try_from(at.as_nanos().get())
                        .map_err(|_| String::from("instant exceeds supported timestamp range"))
                };
                Ok(Self {
                    from: nanos(from)?,
                    to: nanos(to)?,
                })
            }
            SpendWindow::Dates { from, to } => {
                if from > to {
                    return Err(String::from("date window requires from <= to"));
                }
                let civil = |day| {
                    day_str(day)
                        .parse::<jiff::civil::Date>()
                        .map_err(|e| e.to_string())
                };
                let first = civil(from)?;
                let next = civil(to)?.tomorrow().map_err(|e| e.to_string())?;
                let boundary = |date: jiff::civil::Date| {
                    // Начало суток может быть не в 00:00, а сами сутки — не 24 часа.
                    let start = zone
                        .tz()
                        .to_zoned(date.at(0, 0, 0, 0))
                        .and_then(|z| z.start_of_day())
                        .map_err(|e| e.to_string())?;
                    i64::try_from(start.timestamp().as_nanosecond())
                        .map_err(|_| String::from("date exceeds supported timestamp range"))
                };
                Ok(Self {
                    from: boundary(first)?,
                    to: boundary(next)?,
                })
            }
        }
    }
}

/// Сутки счётчиков и строк `spend` в `system.tz` (`09 §1`).
#[data]
pub enum DayZone {
    /// Зона хоста — `system.tz = None`.
    Host,
    Utc,
    /// Постоянное смещение от UTC в секундах, `system.tz = "±HH:MM"`.
    Fixed(i32),
    /// Каноническое имя IANA из встроенной базы зон: сутки режет `eva_day` с учётом DST.
    Zone(String),
}

/// `system.tz`, по которой soul не умеет резать сутки.
#[data(
    error,
    display(
        "system.tz {_0:?} is not supported: use UTC, a fixed offset ±HH:MM or an IANA zone name"
    )
)]
pub struct UnsupportedTz(pub String);

impl DayZone {
    /// Предел смещения ISO 8601.
    const MAX_OFFSET: i32 = 18 * 3600;

    /// Зона суток из `system.tz`: `UTC`, `Etc/UTC`, `±HH:MM` или имя IANA.
    pub fn from_tz(tz: Option<&str>) -> Result<Self, UnsupportedTz> {
        match tz {
            None => Ok(Self::Host),
            Some("UTC" | "Etc/UTC") => Ok(Self::Utc),
            Some(tz) => match Self::offset(tz) {
                Some(secs) => Ok(Self::Fixed(secs)),
                None => TimeZone::get(tz)
                    .ok()
                    .and_then(|zone| zone.iana_name().map(str::to_owned))
                    .map(Self::Zone)
                    .ok_or_else(|| UnsupportedTz(tz.to_owned())),
            },
        }
    }

    /// `±HH:MM` → секунды к востоку от UTC.
    fn offset(tz: &str) -> Option<i32> {
        let (sign, rest) = match tz.as_bytes().first()? {
            b'+' => (1, &tz[1..]),
            b'-' => (-1, &tz[1..]),
            _ => return None,
        };
        let (hours, mins) = rest.split_once(':')?;
        let digits = |s: &str| {
            (s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit()))
                .then(|| s.parse::<i32>().ok())
                .flatten()
        };
        let (hours, mins) = (digits(hours)?, digits(mins)?);
        let secs = hours * 3600 + mins * 60;
        (mins < 60 && secs <= Self::MAX_OFFSET).then_some(sign * secs)
    }

    fn sql(&self, ts: &str) -> String {
        match self {
            Self::Host => format!("date({ts} / 1000000000, 'unixepoch', 'localtime')"),
            Self::Utc => format!("date({ts} / 1000000000, 'unixepoch')"),
            Self::Fixed(secs) => format!("date({ts} / 1000000000 + ({secs}), 'unixepoch')"),
            // Имя пришло из базы зон, кавычек в нём нет; экранирование — страховка.
            Self::Zone(name) => format!("eva_day({ts}, '{}')", name.replace('\'', "''")),
        }
    }

    /// Та же зона для расчётов в коде: границы суток и слоты планировщиков.
    pub fn tz(&self) -> TimeZone {
        match self {
            Self::Host => TimeZone::try_system().unwrap_or(TimeZone::UTC),
            Self::Utc => TimeZone::UTC,
            Self::Fixed(secs) => Offset::from_seconds(*secs).map_or(TimeZone::UTC, TimeZone::fixed),
            Self::Zone(name) => TimeZone::get(name).unwrap_or(TimeZone::UTC),
        }
    }

    fn zoned(&self, at: Timestamp) -> Zoned {
        let ts = jiff::Timestamp::from_nanosecond(i128::from(at.as_nanos().get()))
            .unwrap_or(jiff::Timestamp::MAX);
        ts.to_zoned(self.tz())
    }

    /// Сутки `at` строкой `YYYY-MM-DD`, как их режет `sql`.
    pub fn day(&self, at: Timestamp) -> String {
        self.zoned(at).date().to_string()
    }

    /// Начало календарных суток, в которых лежит `at`.
    pub fn day_start(&self, at: Timestamp) -> Timestamp {
        let zoned = self.zoned(at);
        let start = zoned.start_of_day().unwrap_or_else(|_| zoned.clone());
        from_zoned(&start)
    }

    /// Последнее наступление стеночного времени `time` не позже `now`.
    pub fn latest_at(&self, time: Time, now: Timestamp) -> Timestamp {
        let zoned = self.zoned(now);
        let mut date = zoned.date();
        loop {
            let candidate = self.wall(date, time);
            if candidate <= now {
                return candidate;
            }
            date = date.yesterday().unwrap_or(date);
        }
    }

    /// Последнее наступление `weekday time` не позже `now`.
    pub fn latest_weekly(&self, weekday: Weekday, time: Time, now: Timestamp) -> Timestamp {
        let mut date = self.zoned(now).date();
        // Не позже семи шагов назад день недели совпадёт и слот ляжет до `now`.
        for _ in 0..9 {
            if date.weekday() == weekday {
                let candidate = self.wall(date, time);
                if candidate <= now {
                    return candidate;
                }
            }
            date = date.yesterday().unwrap_or(date);
        }
        now
    }

    /// Первое наступление `weekday time` строго позже `after`.
    pub fn next_weekly(&self, weekday: Weekday, time: Time, after: Timestamp) -> Timestamp {
        let mut date = self.zoned(after).date();
        for _ in 0..9 {
            if date.weekday() == weekday {
                let candidate = self.wall(date, time);
                if candidate > after {
                    return candidate;
                }
            }
            date = date.tomorrow().unwrap_or(date);
        }
        after
    }

    /// Момент `time` в день `date` этой зоны; в разрыве DST — ближайший существующий.
    fn wall(&self, date: jiff::civil::Date, time: Time) -> Timestamp {
        let civil = date.at(
            i8::try_from(time.hours() as u8).unwrap_or(0),
            i8::try_from(time.mins() as u8).unwrap_or(0),
            0,
            0,
        );
        match self.tz().to_zoned(civil) {
            Ok(zoned) => from_zoned(&zoned),
            Err(_) => Timestamp::MIN,
        }
    }
}

fn from_zoned(zoned: &Zoned) -> Timestamp {
    u64::try_from(zoned.timestamp().as_nanosecond())
        .ok()
        .and_then(std::num::NonZeroU64::new)
        .map_or(Timestamp::MIN, Timestamp::from_nanos)
}

/// `eva_day(ts, zone)` — дата `YYYY-MM-DD` наносекунд `ts` в зоне IANA: `localtime`
/// SQLite знает только зону хоста. Зона разбирается раз на стейтмент (aux-данные).
pub fn install_day(conn: &Connection) -> DbResult<()> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    conn.create_scalar_function("eva_day", 2, flags, |ctx| {
        let zone = ctx.get_or_create_aux(1, |name| -> Result<TimeZone, BoxError> {
            Ok(TimeZone::get(name.as_str()?)?)
        })?;
        let ts: i64 = ctx.get(0)?;
        let at = jiff::Timestamp::from_nanosecond(i128::from(ts))
            .map_err(|e| rusqlite::Error::UserFunctionError(e.into()))?;
        Ok(at.to_zoned(TimeZone::clone(&zone)).date().to_string())
    })?;
    Ok(())
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[data]
pub struct ToolUsage {
    pub tool_id: ToolId,
    pub name: Option<String>,
    pub calls: u64,
    pub turns_with_call: u64,
    pub loads: u64,
    pub errors: u64,
    pub ends_turn: u64,
}

#[data]
pub struct SpendGroup {
    pub key: String,
    pub currency: Option<String>,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cache_write_long: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cost: Option<f64>,
}

#[data]
#[derive(Default)]
pub struct CacheTotals {
    pub step_cache_read: u64,
    pub step_cache_write: u64,
    pub step_prompt_total: u64,
    pub context_edits: u64,
    pub keep_alive: u64,
    pub keep_alive_cost: Vec<MoneyTotal>,
    pub breaks: Vec<(BreakReason, u64)>,
    /// `(model, prompt_total)` шагов без чтения кэша: холодный ли — решают caps модели.
    pub cold_candidates: Vec<(ModelId, u64)>,
    /// `Σ cache_read` строк с ценой из прайс-бука, по модели.
    pub pricebook_cache_read: Vec<(ModelId, u64)>,
}

#[data]
pub struct SeriesRow {
    pub day: Date,
    pub key: String,
    pub currency: Option<String>,
    pub cost: Option<f64>,
    pub prompt_total: u64,
    pub output: u64,
}

#[data]
pub struct MoneyTotal {
    pub currency: String,
    pub cost: f64,
}

#[data]
pub struct ModelTotals {
    pub model: ModelId,
    pub cost: Vec<MoneyTotal>,
    pub prompt_total: u64,
    pub output: u64,
}

#[data]
pub struct TurnTotals {
    pub turn: TurnId,
    pub cost: Vec<MoneyTotal>,
    pub prompt_total: u64,
    pub cache_read: u64,
}

#[data]
pub struct ChatSpendRow {
    pub total: Vec<MoneyTotal>,
    pub by_model: Vec<ModelTotals>,
    pub last_turn: Option<TurnTotals>,
    pub cache_read: u64,
    pub prompt_total: u64,
}

#[data]
pub struct CounterRow {
    pub metric: String,
    pub key: String,
    pub count: u64,
}

#[data]
pub struct ModuleRow {
    pub module: ModuleId,
    /// Есть живой выданный токен.
    pub live_token: bool,
    pub state: State,
    pub since: Timestamp,
    /// msgpack `Capabilities`, как в колонке.
    pub capabilities: Vec<u8>,
}

#[data(copy)]
pub struct IndexBacklog {
    pub pending: u64,
    pub oldest: Option<Timestamp>,
}

pub struct Stats<'c>(pub &'c Connection);

impl Stats<'_> {
    /// +1 к `metric{key}` за сутки `at` в зоне `zone`; `key = ""` — без разреза.
    pub fn bump(&self, metric: Metric, key: &str, at: Timestamp, zone: &DayZone) -> DbResult<()> {
        self.bump_by(metric, key, 1, at, zone)
    }

    /// `+n`: счётчики строк (`gc.purged{entity}`), не вызовов.
    pub fn bump_by(
        &self,
        metric: Metric,
        key: &str,
        n: u64,
        at: Timestamp,
        zone: &DayZone,
    ) -> DbResult<()> {
        if n == 0 {
            return Ok(());
        }
        self.0.execute(
            &format!(
                "INSERT INTO health_stats (day, metric, key, count) VALUES ({}, ?2, ?3, ?4)
                 ON CONFLICT (day, metric, key) DO UPDATE SET count = count + excluded.count",
                zone.sql("?1")
            ),
            (Ts(at), metric.as_str(), key, saturate(n)),
        )?;
        Ok(())
    }

    /// Строки по тулам, сортировка по `calls`; `name` — текущий канон живого тула.
    pub fn tools_usage(
        &self,
        scope: &ScopeFilter,
        period: Period,
        top: u8,
    ) -> DbResult<Vec<ToolUsage>> {
        let sql = format!(
            "SELECT s.tool_id, CASE WHEN t.retired_at IS NULL THEN t.canonical END AS name,
                    SUM(s.calls) AS calls, SUM(s.turns_with_call) AS turns_with_call,
                    SUM(s.loads) AS loads, SUM(s.errors) AS errors, SUM(s.ends_turn) AS ends_turn
               FROM tool_stats s LEFT JOIN tools t ON t.tool_id = s.tool_id
              WHERE s.day BETWEEN :from AND :to AND {}
              GROUP BY s.tool_id ORDER BY calls DESC, s.tool_id LIMIT :top",
            scope.sql("s.chat_id")
        );
        let mut stmt = self.0.prepare(&sql)?;
        let mut rows: Vec<ToolUsage> = stmt
            .query_map(
                named_params! {
                    ":from": period.first_day(),
                    ":to": period.last_day(),
                    ":target": scope.target(),
                    ":top": top,
                },
                |row| {
                    Ok(ToolUsage {
                        tool_id: ToolId(row.get("tool_id")?),
                        name: row.get("name")?,
                        calls: count(row, "calls")?,
                        turns_with_call: count(row, "turns_with_call")?,
                        loads: count(row, "loads")?,
                        errors: count(row, "errors")?,
                        ends_turn: count(row, "ends_turn")?,
                    })
                },
            )?
            .collect::<Result<_, _>>()?;
        for row in &mut rows {
            if row.name.is_some() {
                row.name = crate::db::tools::Tools(self.0).canonical_name(row.tool_id)?;
            }
        }
        Ok(rows)
    }

    /// Знаменатель покрытия: `Σ chat_stats.turns` в том же разрезе.
    pub fn tools_totals(&self, scope: &ScopeFilter, period: Period) -> DbResult<(u64, u64)> {
        let sql = format!(
            "SELECT COALESCE(SUM(s.calls), 0) AS calls, COUNT(DISTINCT s.tool_id) AS distinct_tools
               FROM tool_stats s WHERE s.day BETWEEN :from AND :to AND {}",
            scope.sql("s.chat_id"),
        );
        Ok(self.0.query_row(
            &sql,
            named_params! {
                ":from": period.first_day(), ":to": period.last_day(), ":target": scope.target(),
            },
            |row| Ok((count(row, "calls")?, count(row, "distinct_tools")?)),
        )?)
    }

    /// Знаменатель покрытия: `Σ chat_stats.turns` в том же разрезе.
    pub fn turns(&self, scope: &ScopeFilter, period: Period) -> DbResult<u64> {
        let sql = format!(
            "SELECT COALESCE(SUM(s.turns), 0) AS turns FROM chat_stats s
              WHERE s.day BETWEEN :from AND :to AND {}",
            scope.sql("s.chat_id")
        );
        Ok(self.0.query_row(
            &sql,
            named_params! {
                ":from": period.first_day(),
                ":to": period.last_day(),
                ":target": scope.target(),
            },
            |row| count(row, "turns"),
        )?)
    }

    /// Строки `spend` разреза и периода: общий `WHERE` запросов ниже.
    fn spend_where(scope: &ScopeFilter) -> String {
        format!("s.ts >= :lo AND s.ts < :hi AND {}", scope.sql("s.chat_id"))
    }

    fn spend_params(scope: &ScopeFilter, period: SpendPeriod) -> [(&'static str, Value); 3] {
        [
            (":lo", Value::Integer(period.from)),
            (":hi", Value::Integer(period.to)),
            (":target", scope.target()),
        ]
    }

    /// Суммы по группе и валюте: смешанные валюты не складываются (`09 §2.1`).
    pub fn unpriced(&self, scope: &ScopeFilter, period: SpendPeriod) -> DbResult<u64> {
        let sql = format!(
            "SELECT COUNT(*) AS n FROM spend s WHERE s.cost IS NULL AND {}",
            Self::spend_where(scope)
        );
        Ok(self.0.query_row(
            &sql,
            named(&Self::spend_params(scope, period)).as_slice(),
            |row| count(row, "n"),
        )?)
    }

    /// Суммы по группе и валюте: смешанные валюты не складываются (`09 §2.1`).
    pub fn spend(
        &self,
        scope: &ScopeFilter,
        period: SpendPeriod,
        group_by: GroupBy,
        zone: &DayZone,
    ) -> DbResult<Vec<SpendGroup>> {
        let sql = format!(
            "SELECT {key} AS key, s.currency,
                    SUM(s.input) AS input, SUM(s.cache_read) AS cache_read,
                    SUM(s.cache_write) AS cache_write, SUM(s.cache_write_long) AS cache_write_long,
                    SUM(s.output) AS output, SUM(s.reasoning) AS reasoning, SUM(s.cost) AS cost
               FROM spend s WHERE {filter}
              GROUP BY 1, 2 ORDER BY 1, 2",
            key = group_key(group_by, zone),
            filter = Self::spend_where(scope),
        );
        let params = Self::spend_params(scope, period);
        let mut stmt = self.0.prepare(&sql)?;
        let rows = stmt
            .query_map(named(&params).as_slice(), |row| {
                Ok(SpendGroup {
                    key: row.get("key")?,
                    currency: row.get("currency")?,
                    input: count(row, "input")?,
                    cache_read: count(row, "cache_read")?,
                    cache_write: count(row, "cache_write")?,
                    cache_write_long: count(row, "cache_write_long")?,
                    output: count(row, "output")?,
                    reasoning: count(row, "reasoning")?,
                    cost: row.get("cost")?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Сырьё метрик кэша (`09 §3`) того же разреза.
    pub fn cache(&self, scope: &ScopeFilter, period: SpendPeriod) -> DbResult<CacheTotals> {
        let filter = Self::spend_where(scope);
        let params = Self::spend_params(scope, period);
        let params = named(&params);

        let mut totals = self.0.query_row(
            &format!(
                "SELECT COALESCE(SUM(CASE WHEN s.op = 'step' THEN s.cache_read END), 0) AS cr,
                        COALESCE(SUM(CASE WHEN s.op = 'step' THEN s.cache_write END), 0) AS cw,
                        COALESCE(SUM(CASE WHEN s.op = 'step'
                                          THEN s.input + s.cache_read + s.cache_write END), 0) AS pt,
                        COALESCE(SUM(s.context_edits_applied), 0) AS edits,
                        COUNT(CASE WHEN s.op = 'keep_alive' THEN 1 END) AS ka
                   FROM spend s WHERE {filter}"
            ),
            params.as_slice(),
            |row| {
                Ok(CacheTotals {
                    step_cache_read: count(row, "cr")?,
                    step_cache_write: count(row, "cw")?,
                    step_prompt_total: count(row, "pt")?,
                    context_edits: count(row, "edits")?,
                    keep_alive: count(row, "ka")?,
                    ..CacheTotals::default()
                })
            },
        )?;

        let mut stmt = self.0.prepare(&format!(
            "SELECT s.currency, SUM(s.cost) AS cost FROM spend s
              WHERE {filter} AND s.op = 'keep_alive' AND s.cost IS NOT NULL
                AND s.currency IS NOT NULL GROUP BY s.currency ORDER BY s.currency"
        ))?;
        totals.keep_alive_cost = stmt
            .query_map(params.as_slice(), |row| {
                Ok(MoneyTotal {
                    currency: row.get("currency")?,
                    cost: row.get("cost")?,
                })
            })?
            .collect::<Result<_, _>>()?;

        let mut stmt = self.0.prepare(&format!(
            "SELECT s.cache_break_reason AS reason, COUNT(*) AS n FROM spend s
              WHERE {filter} AND s.cache_break_reason IS NOT NULL
                AND s.cache_break_reason NOT IN ('first_request', 'segment_reset')
              GROUP BY 1 ORDER BY 1"
        ))?;
        totals.breaks = stmt
            .query_map(params.as_slice(), |row| {
                Ok((
                    row.get::<_, Text<BreakReason>>("reason")?.0,
                    count(row, "n")?,
                ))
            })?
            .collect::<Result<_, _>>()?;

        let mut stmt = self.0.prepare(&format!(
            "SELECT s.model AS model, s.input + s.cache_read + s.cache_write AS pt FROM spend s
              WHERE {filter} AND s.op = 'step' AND s.cache_read = 0"
        ))?;
        totals.cold_candidates = stmt
            .query_map(params.as_slice(), |row| {
                Ok((ModelId(row.get("model")?), count(row, "pt")?))
            })?
            .collect::<Result<_, _>>()?;

        let mut stmt = self.0.prepare(&format!(
            "SELECT s.model AS model, SUM(s.cache_read) AS cr FROM spend s
              WHERE {filter} AND s.cost_source = 'pricebook' GROUP BY 1 ORDER BY 1"
        ))?;
        totals.pricebook_cache_read = stmt
            .query_map(params.as_slice(), |row| {
                Ok((ModelId(row.get("model")?), count(row, "cr")?))
            })?
            .collect::<Result<_, _>>()?;

        Ok(totals)
    }

    /// Ряды по дням для графиков.
    pub fn series(
        &self,
        scope: &ScopeFilter,
        period: SpendPeriod,
        group_by: GroupBy,
        zone: &DayZone,
    ) -> DbResult<Vec<SeriesRow>> {
        let sql = format!(
            "SELECT {day} AS day, {key} AS key, s.currency, SUM(s.cost) AS cost,
                    SUM(s.input + s.cache_read + s.cache_write) AS pt, SUM(s.output) AS output
               FROM spend s WHERE {filter}
              GROUP BY 1, 2, 3 ORDER BY 1, 2, 3",
            day = zone.sql("s.ts"),
            key = group_key(group_by, zone),
            filter = Self::spend_where(scope),
        );
        let params = Self::spend_params(scope, period);
        let mut stmt = self.0.prepare(&sql)?;
        let raw = stmt
            .query_map(named(&params).as_slice(), |row| {
                Ok((
                    row.get::<_, String>("day")?,
                    SeriesRow {
                        day: Date::MIN,
                        key: row.get("key")?,
                        currency: row.get("currency")?,
                        cost: row.get("cost")?,
                        prompt_total: count(row, "pt")?,
                        output: count(row, "output")?,
                    },
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        raw.into_iter()
            .map(|(day, row)| {
                Ok(SeriesRow {
                    day: parse_day(&day)?,
                    ..row
                })
            })
            .collect()
    }

    /// Расход чата за всё время (`stats.chat_spend`).
    pub fn chat_spend(&self, chat: ChatId) -> DbResult<ChatSpendRow> {
        let id = chat.get();
        let (cache_read, prompt_total) = self.0.query_row(
            "SELECT COALESCE(SUM(CASE WHEN op = 'step' THEN cache_read END), 0) AS cr,
                    COALESCE(SUM(CASE WHEN op = 'step' THEN input + cache_read + cache_write END), 0) AS pt
               FROM spend WHERE chat_id = ?1",
            [id],
            |row| Ok((count(row, "cr")?, count(row, "pt")?)),
        )?;

        let mut stmt = self.0.prepare(
            "SELECT model, currency, SUM(cost) AS cost,
                    SUM(input + cache_read + cache_write) AS pt, SUM(output) AS output
               FROM spend WHERE chat_id = ?1 GROUP BY model, currency ORDER BY model, currency",
        )?;
        let rows = stmt.query_map([id], |row| {
            Ok((
                row.get::<_, String>("model")?,
                row.get::<_, Option<String>>("currency")?,
                row.get::<_, Option<f64>>("cost")?,
                count(row, "pt")?,
                count(row, "output")?,
            ))
        })?;
        let mut by_model = BTreeMap::<String, ModelTotals>::new();
        let mut total = BTreeMap::<String, f64>::new();
        for row in rows {
            let (model, currency, cost, prompt_total, output) = row?;
            let model = by_model
                .entry(model.clone())
                .or_insert_with(|| ModelTotals {
                    model: ModelId(model),
                    cost: Vec::new(),
                    prompt_total: 0,
                    output: 0,
                });
            model.prompt_total += prompt_total;
            model.output += output;
            if let (Some(currency), Some(cost)) = (currency, cost) {
                *total.entry(currency.clone()).or_default() += cost;
                model.cost.push(MoneyTotal { currency, cost });
            }
        }

        let last: Option<String> = self
            .0
            .query_row(
                "SELECT turn_id FROM spend WHERE chat_id = ?1 AND turn_id IS NOT NULL
                  ORDER BY ts DESC, id DESC LIMIT 1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        let last_turn = if let Some(turn) = last {
            let mut stmt = self.0.prepare(
                "SELECT currency, SUM(cost) AS cost,
                        SUM(input + cache_read + cache_write) AS pt, SUM(cache_read) AS cr
                   FROM spend WHERE chat_id = ?1 AND turn_id = ?2
                  GROUP BY currency ORDER BY currency",
            )?;
            let rows = stmt.query_map((id, &turn), |row| {
                Ok((
                    row.get::<_, Option<String>>("currency")?,
                    row.get::<_, Option<f64>>("cost")?,
                    count(row, "pt")?,
                    count(row, "cr")?,
                ))
            })?;
            let mut totals = TurnTotals {
                turn: TurnId(turn),
                cost: Vec::new(),
                prompt_total: 0,
                cache_read: 0,
            };
            for row in rows {
                let (currency, cost, prompt_total, cache_read) = row?;
                totals.prompt_total += prompt_total;
                totals.cache_read += cache_read;
                if let (Some(currency), Some(cost)) = (currency, cost) {
                    totals.cost.push(MoneyTotal { currency, cost });
                }
            }
            Some(totals)
        } else {
            None
        };

        Ok(ChatSpendRow {
            total: total
                .into_iter()
                .map(|(currency, cost)| MoneyTotal { currency, cost })
                .collect(),
            by_model: by_model.into_values().collect(),
            last_turn,
            cache_read,
            prompt_total,
        })
    }

    /// Ненулевые счётчики `health_stats` за период.
    pub fn counters(&self, period: Period) -> DbResult<Vec<CounterRow>> {
        let mut stmt = self.0.prepare(
            "SELECT metric, key, SUM(count) AS n FROM health_stats
              WHERE day BETWEEN ?1 AND ?2
              GROUP BY metric, key HAVING SUM(count) > 0 ORDER BY metric, key",
        )?;
        let rows = stmt
            .query_map((period.first_day(), period.last_day()), |row| {
                Ok(CounterRow {
                    metric: row.get("metric")?,
                    key: row.get("key")?,
                    count: count(row, "n")?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Незабытые модули; `live_token` — живой токен из `module_tokens`.
    pub fn modules(&self) -> DbResult<Vec<ModuleRow>> {
        let mut stmt = self.0.prepare(
            "SELECT m.name, m.state, m.capabilities, m.last_seen_at,
                    EXISTS (SELECT 1 FROM module_tokens t
                            WHERE t.module = m.name AND t.revoked_at IS NULL) AS live_token
               FROM modules m WHERE m.state <> 'forgotten' ORDER BY m.name",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ModuleRow {
                    module: ModuleId(row.get("name")?),
                    live_token: row.get("live_token")?,
                    state: row.get::<_, Text<State>>("state")?.0,
                    since: row.get::<_, Ts>("last_seen_at")?.0,
                    capabilities: row.get("capabilities")?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Размер файла базы.
    pub fn db_bytes(&self) -> DbResult<u64> {
        Ok(self.0.query_row(
            "SELECT p.page_count * s.page_size AS n FROM pragma_page_count() p, pragma_page_size() s",
            [],
            |row| count(row, "n"),
        )?)
    }

    /// Реплики, ждущие индексации поиска (`05 §7.1`), и возраст старейшей.
    pub fn index_backlog(&self) -> DbResult<IndexBacklog> {
        Ok(self.0.query_row(
            "SELECT COUNT(*) AS n, MIN(e.created_at) AS oldest
               FROM messages m JOIN events e ON e.id = m.event_id WHERE m.indexed_at IS NULL",
            [],
            |row| {
                Ok(IndexBacklog {
                    pending: count(row, "n")?,
                    oldest: row.get::<_, Option<Ts>>("oldest")?.map(|t| t.0),
                })
            },
        )?)
    }

    /// Очередь эмбеддера: текущие строки без вектора (`03 §8`).
    pub fn embed_pending(&self) -> DbResult<u64> {
        Ok(self.0.query_row(
            "SELECT COUNT(*) AS n FROM memory WHERE valid_to IS NULL AND embedding IS NULL",
            [],
            |row| count(row, "n"),
        )?)
    }

    /// Строка `spend` на usage-чанк (`09 §2`).
    pub fn insert_spend(&self, row: &SpendRow) -> DbResult<()> {
        let (cost, currency, cost_source) = match &row.cost {
            Some(cost) => (
                Some(cost.amount),
                Some(cost.currency.as_str()),
                match cost.source {
                    CostSource::Provider => "provider",
                    CostSource::Pricebook => "pricebook",
                },
            ),
            None => (None, None, "none"),
        };
        self.0.execute(
            "INSERT INTO spend (ts, model, source, op, module, chat_id, turn_id, run_id,
                                input, cache_read, cache_write, cache_write_long, output, reasoning,
                                context_edits_applied, cache_break_reason, cost, currency, cost_source)
             VALUES (:ts, :model, :source, :op, :module, :chat, :turn, :run,
                     :input, :cache_read, :cache_write, :cache_write_long, :output, :reasoning,
                     :edits, :break_reason, :cost, :currency, :cost_source)",
            named_params! {
                ":ts": Ts(row.ts),
                ":model": row.model.as_str(),
                ":source": Text(row.source),
                ":op": Text(row.op),
                ":module": row.module.as_ref().map(ModuleId::as_str),
                ":chat": row.chat.map(ChatId::get),
                ":turn": row.turn.as_ref().map(TurnId::as_str),
                ":run": row.run.as_ref().map(RunId::as_str),
                ":input": saturate(row.input),
                ":cache_read": saturate(row.cache_read),
                ":cache_write": saturate(row.cache_write),
                ":cache_write_long": saturate(row.cache_write_long),
                ":output": saturate(row.output),
                ":reasoning": saturate(row.reasoning),
                ":edits": row.context_edits_applied,
                ":break_reason": row.cache_break_reason.map(Text),
                ":cost": cost,
                ":currency": currency,
                ":cost_source": cost_source,
            },
        )?;
        Ok(())
    }

    /// `Σ spend.cost` запуска крона или ночи; `None` — ни одной оценённой строки.
    pub fn run_cost(&self, run: &RunId) -> DbResult<Option<f64>> {
        Ok(self.0.query_row(
            "SELECT SUM(cost) FROM spend WHERE run_id = ?1",
            [run.as_str()],
            |row| row.get(0),
        )?)
    }

    /// Итог тура одной транзакцией (`09 §1.3`): бакеты суток старта тура в зоне `zone`.
    pub fn flush_turn(
        &self,
        chat: ChatId,
        started_at: Timestamp,
        tools: &[(ToolId, ToolDelta)],
        delta: &ChatDelta,
        zone: &DayZone,
    ) -> DbResult<()> {
        let day = zone.sql(":at");
        self.0.execute(
            &format!(
                "INSERT INTO chat_stats (day, chat_id, turns, steps, user_messages, assistant_messages, cancelled)
                 VALUES ({day}, :chat, :turns, :steps, :user, :assistant, :cancelled)
                 ON CONFLICT (day, chat_id) DO UPDATE SET
                   turns = turns + excluded.turns, steps = steps + excluded.steps,
                   user_messages = user_messages + excluded.user_messages,
                   assistant_messages = assistant_messages + excluded.assistant_messages,
                   cancelled = cancelled + excluded.cancelled"
            ),
            named_params! {
                ":at": Ts(started_at),
                ":chat": chat.get(),
                ":turns": delta.turns,
                ":steps": delta.steps,
                ":user": delta.user_messages,
                ":assistant": delta.assistant_messages,
                ":cancelled": delta.cancelled,
            },
        )?;
        let mut stmt = self.0.prepare(&format!(
            "INSERT INTO tool_stats (day, chat_id, tool_id, calls, turns_with_call, loads, errors, ends_turn, last_at)
             VALUES ({day}, :chat, :tool, :calls, :with_call, :loads, :errors, :ends_turn, :at)
             ON CONFLICT (day, chat_id, tool_id) DO UPDATE SET
               calls = calls + excluded.calls, turns_with_call = turns_with_call + excluded.turns_with_call,
               loads = loads + excluded.loads, errors = errors + excluded.errors,
               ends_turn = ends_turn + excluded.ends_turn, last_at = MAX(last_at, excluded.last_at)"
        ))?;
        for (tool, d) in tools {
            stmt.execute(named_params! {
                ":at": Ts(started_at),
                ":chat": chat.get(),
                ":tool": tool.get(),
                ":calls": d.calls,
                ":with_call": u32::from(d.calls > 0),
                ":loads": d.loads,
                ":errors": d.errors,
                ":ends_turn": d.ends_turn,
            })?;
        }
        Ok(())
    }

    /// Gauge `cron_fire_lag_last{job}`: последний плановый запуск каждой живой джобы.
    pub fn cron_lags(&self) -> DbResult<Vec<CronLag>> {
        let mut stmt = self.0.prepare(
            "SELECT j.name, r.started - r.due AS lag FROM cron_jobs j
               JOIN cron_runs r ON r.run_id = (SELECT run_id FROM cron_runs
                                                WHERE job_id = j.id AND due IS NOT NULL
                                                ORDER BY started DESC, run_id DESC LIMIT 1)
              ORDER BY j.name",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let lag: i64 = row.get("lag")?;
                Ok(CronLag {
                    job: row.get("name")?,
                    lag: Duration::from_nanos(u64::try_from(lag).unwrap_or(0)),
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Ретеншн `09 §8`: бакеты раньше суток `keep_from`, `spend` раньше `spend_from`.
    pub fn purge(&self, keep_from: &str, spend_from: Timestamp) -> DbResult<StatsPurged> {
        let mut purged = StatsPurged::default();
        for (table, slot) in [
            ("tool_stats", &mut purged.tool_stats),
            ("chat_stats", &mut purged.chat_stats),
            ("health_stats", &mut purged.health_stats),
        ] {
            *slot = self
                .0
                .execute(&format!("DELETE FROM {table} WHERE day < ?1"), [keep_from])?;
        }
        purged.spend = self
            .0
            .execute("DELETE FROM spend WHERE ts < ?1", [Ts(spend_from)])?;
        Ok(purged)
    }
}

/// Счётчик провайдера в INTEGER-колонке: за `i64::MAX` не бывает, но и не паникуем.
fn saturate(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Выражение группы `GroupBy` над `spend s`.
fn group_key(group_by: GroupBy, zone: &DayZone) -> String {
    match group_by {
        GroupBy::Model => String::from("s.model"),
        GroupBy::Source => String::from("s.source"),
        GroupBy::Op => String::from("s.op"),
        GroupBy::Chat => {
            String::from("CASE WHEN s.chat_id IS NULL THEN 'none' ELSE 'c' || s.chat_id END")
        }
        GroupBy::Day => zone.sql("s.ts"),
    }
}

fn named<'p>(params: &'p [(&'static str, Value)]) -> Vec<(&'static str, &'p dyn rusqlite::ToSql)> {
    params
        .iter()
        .map(|(name, value)| (*name, value as &dyn rusqlite::ToSql))
        .collect()
}

/// Неотрицательная сумма или счётчик: `u64` у rusqlite без фичи не читается.
fn count(row: &Row<'_>, column: &str) -> rusqlite::Result<u64> {
    let raw: i64 = row.get(column)?;
    u64::try_from(raw).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, raw))
}

/// `Date` → `'YYYY-MM-DD'` колонки `day` (`10 §1`).
pub fn day_str(date: Date) -> String {
    let (y, m, d) = civil_from_days(i64::from(date.days()));
    format!("{y:04}-{m:02}-{d:02}")
}

/// `'YYYY-MM-DD'` → `Date`; иное — порча строки.
pub fn parse_day(day: &str) -> DbResult<Date> {
    let corrupt = || DbError::corrupt(format_args!("day {day:?} is not YYYY-MM-DD"));
    let mut parts = day.splitn(3, '-').map(str::parse::<i64>);
    let (Some(Ok(y)), Some(Ok(m)), Some(Ok(d))) = (parts.next(), parts.next(), parts.next()) else {
        return Err(corrupt());
    };
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(corrupt());
    }
    let days = days_from_civil(y, m, d);
    // Обратный пересчёт ловит 31 февраля.
    if civil_from_days(days) != (y, m, d) {
        return Err(corrupt());
    }
    u16::try_from(days)
        .map(Date::from_days)
        .map_err(|_| corrupt())
}

/// Дни от 1970-01-01 → (год, месяц, день), пролептический григорианский.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use super::*;

    const SEC: u64 = 1_000_000_000;

    fn ts(secs: u64) -> Timestamp {
        Timestamp::from_nanos(NonZeroU64::new(secs * SEC).expect("non-zero"))
    }

    fn time(h: u8, m: u8) -> Time {
        Time::new(
            eva::time::Hours::new(h).expect("hours"),
            eva::time::Mins::new(m).expect("mins"),
        )
    }

    #[test]
    fn latest_at_is_the_last_wall_time_not_after_now() {
        let utc = DayZone::Utc;
        // 2026-07-01T02:00Z.
        let now = ts(1_782_871_200);
        let midnight = utc.latest_at(time(0, 0), now);
        assert_eq!(utc.day(midnight), "2026-07-01");
        assert_eq!(
            now.as_nanos().get() - midnight.as_nanos().get(),
            7_200 * SEC
        );
        let three = utc.latest_at(time(3, 0), now);
        assert_eq!(utc.day(three), "2026-06-30");
        assert_eq!(utc.latest_at(time(2, 0), now), now, "exact hit counts");

        // Нью-Йорк (EDT, −4): 02:00Z — ещё 30 июня, полночь местная — 04:00Z 30 июня.
        let ny = DayZone::Zone(String::from("America/New_York"));
        assert_eq!(ny.day(now), "2026-06-30");
        let local_midnight = ny.latest_at(time(0, 0), now);
        assert_eq!(utc.day(local_midnight), "2026-06-30");
        assert_eq!(
            local_midnight.as_nanos().get() % (86_400 * SEC),
            4 * 3_600 * SEC
        );
        assert_eq!(ny.day_start(now), local_midnight);
    }

    #[test]
    fn weekly_slots_wrap_around_the_week() {
        let utc = DayZone::Utc;
        // 2026-07-01 is a Wednesday.
        let now = ts(1_782_871_200);
        let last = utc.latest_weekly(Weekday::Sunday, time(3, 0), now);
        assert_eq!(utc.day(last), "2026-06-28");
        let next = utc.next_weekly(Weekday::Sunday, time(3, 0), now);
        assert_eq!(utc.day(next), "2026-07-05");
        assert_eq!(utc.next_weekly(Weekday::Sunday, time(3, 0), last), next);
        let today = utc.latest_weekly(Weekday::Wednesday, time(1, 0), now);
        assert_eq!(utc.day(today), "2026-07-01");
        let not_yet = utc.latest_weekly(Weekday::Wednesday, time(5, 0), now);
        assert_eq!(utc.day(not_yet), "2026-06-24");
    }

    #[test]
    fn metric_names_are_unique_and_dotted() {
        let mut names: Vec<_> = Metric::ALL.iter().map(|m| m.as_str()).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total);
        assert!(names.iter().all(|n| {
            n.chars()
                .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_')
        }));
    }

    #[test]
    fn day_zone_accepts_utc_offsets_and_iana_names() {
        assert_eq!(DayZone::from_tz(None), Ok(DayZone::Host));
        assert_eq!(DayZone::from_tz(Some("Etc/UTC")), Ok(DayZone::Utc));
        assert_eq!(DayZone::from_tz(Some("+03:00")), Ok(DayZone::Fixed(10_800)));
        assert_eq!(
            DayZone::from_tz(Some("-05:30")),
            Ok(DayZone::Fixed(-19_800))
        );
        assert_eq!(
            DayZone::from_tz(Some("europe/moscow")),
            Ok(DayZone::Zone(String::from("Europe/Moscow")))
        );
        for bad in [
            "Mars/Olympus",
            "Europe/Moscow'; DROP TABLE spend; --",
            "+3:00",
            "+03:60",
            "+19:00",
            "03:00",
            "+03",
            "",
        ] {
            assert!(DayZone::from_tz(Some(bad)).is_err(), "{bad:?}");
        }
    }
}
