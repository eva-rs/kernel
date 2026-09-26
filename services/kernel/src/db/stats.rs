//! Таблицы `tool_stats`, `chat_stats`, `spend`, `health_stats` (`09`, `10 §2.9`)
//! и чтения чужих таблиц для живых gauge'ей (`09 §4`).

use eva::{
    data,
    time::{Date, Timestamp},
};
use rusqlite::{Connection, OptionalExtension as _, Row, named_params, types::Value};

use eva_protocol::types::{
    ChatId, FolderId, ModelId, ModuleId, SurfaceId, ToolId, TurnId,
    module::{State, TokenKind},
    stats::{BreakReason, GroupBy},
};

use crate::db::{
    DbError, DbResult,
    sql::{Text, Ts},
};

const DAY_NANOS: i64 = 86_400 * 1_000_000_000;

/// Счётчик здоровья `health_stats` (`09 §5`): список закрыт, фича добавляет
/// свой вариант; имя в БД — `as_str`.
#[data(copy)]
pub enum Metric {
    ModulesConnected,
    ModulesDisconnected,
    ModulesReconnects,
    PersonsCreated,
    ChatsTrustMoves,
    InferenceErrors,
    SpendUnpriced,
}

impl Metric {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModulesConnected => "modules.connected",
            Self::ModulesDisconnected => "modules.disconnected",
            Self::ModulesReconnects => "modules.reconnects",
            Self::PersonsCreated => "persons.created",
            Self::ChatsTrustMoves => "chats.trust_moves",
            Self::InferenceErrors => "inference.errors",
            Self::SpendUnpriced => "spend.unpriced",
        }
    }
}

impl DayZone {
    /// Зона суток из `system.tz`: без tz-базы ядро знает только хост и UTC.
    pub fn from_tz(tz: Option<&str>) -> Self {
        match tz {
            Some("UTC" | "Etc/UTC") => Self::Utc,
            None => Self::Host,
            Some(tz) => {
                log::warn!(tz; "system.tz is not supported without a timezone database, using the host zone");
                Self::Host
            }
        }
    }
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

    /// Грубое окно по `ts` для индекса `spend_ts`: зона сдвигает сутки не больше чем на день.
    fn ts_window(self) -> (i64, i64) {
        let lo = (i64::from(self.from.days()) - 1) * DAY_NANOS;
        let hi = (i64::from(self.to.days()) + 2) * DAY_NANOS;
        (lo, hi)
    }
}

/// Сутки строки `spend` в зоне суток статистики (`09 §1`).
#[data(copy)]
pub enum DayZone {
    /// Зона хоста — `system.tz = None`.
    Host,
    Utc,
}

impl DayZone {
    fn sql(self, ts: &str) -> String {
        match self {
            Self::Host => format!("date({ts} / 1000000000, 'unixepoch', 'localtime')"),
            Self::Utc => format!("date({ts} / 1000000000, 'unixepoch')"),
        }
    }
}

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
    pub keep_alive_cost: Option<f64>,
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
    pub cost: Option<f64>,
    pub prompt_total: u64,
    pub output: u64,
}

#[data]
pub struct ModelTotals {
    pub model: ModelId,
    pub cost: Option<f64>,
    pub prompt_total: u64,
    pub output: u64,
}

#[data]
pub struct TurnTotals {
    pub turn: TurnId,
    pub cost: Option<f64>,
    pub prompt_total: u64,
    pub cache_read: u64,
}

#[data]
pub struct ChatSpendRow {
    /// Валюта новейшей оценённой строки: итоги в других не смешиваются.
    pub currency: Option<String>,
    pub total: Option<f64>,
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
    pub kind: Option<TokenKind>,
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
    pub fn bump(&self, metric: Metric, key: &str, at: Timestamp, zone: DayZone) -> DbResult<()> {
        self.0.execute(
            &format!(
                "INSERT INTO health_stats (day, metric, key, count) VALUES ({}, ?2, ?3, 1)
                 ON CONFLICT (day, metric, key) DO UPDATE SET count = count + 1",
                zone.sql("?1")
            ),
            (Ts(at), metric.as_str(), key),
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
        let rows = stmt
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
        Ok(rows)
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
    fn spend_where(scope: &ScopeFilter, zone: DayZone) -> String {
        format!(
            "s.ts >= :lo AND s.ts < :hi AND {} BETWEEN :from AND :to AND {}",
            zone.sql("s.ts"),
            scope.sql("s.chat_id")
        )
    }

    fn spend_params(scope: &ScopeFilter, period: Period) -> [(&'static str, Value); 5] {
        let (lo, hi) = period.ts_window();
        [
            (":lo", Value::Integer(lo)),
            (":hi", Value::Integer(hi)),
            (":from", Value::Text(period.first_day())),
            (":to", Value::Text(period.last_day())),
            (":target", scope.target()),
        ]
    }

    /// Суммы по группе и валюте: смешанные валюты не складываются (`09 §2.1`).
    pub fn spend(
        &self,
        scope: &ScopeFilter,
        period: Period,
        group_by: GroupBy,
        zone: DayZone,
    ) -> DbResult<Vec<SpendGroup>> {
        let sql = format!(
            "SELECT {key} AS key, s.currency,
                    SUM(s.input) AS input, SUM(s.cache_read) AS cache_read,
                    SUM(s.cache_write) AS cache_write, SUM(s.cache_write_long) AS cache_write_long,
                    SUM(s.output) AS output, SUM(s.reasoning) AS reasoning, SUM(s.cost) AS cost
               FROM spend s WHERE {filter}
              GROUP BY 1, 2 ORDER BY 1, 2",
            key = group_key(group_by, zone),
            filter = Self::spend_where(scope, zone),
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
    pub fn cache(
        &self,
        scope: &ScopeFilter,
        period: Period,
        zone: DayZone,
    ) -> DbResult<CacheTotals> {
        let filter = Self::spend_where(scope, zone);
        let params = Self::spend_params(scope, period);
        let params = named(&params);

        let mut totals = self.0.query_row(
            &format!(
                "SELECT COALESCE(SUM(CASE WHEN s.op = 'step' THEN s.cache_read END), 0) AS cr,
                        COALESCE(SUM(CASE WHEN s.op = 'step' THEN s.cache_write END), 0) AS cw,
                        COALESCE(SUM(CASE WHEN s.op = 'step'
                                          THEN s.input + s.cache_read + s.cache_write END), 0) AS pt,
                        COALESCE(SUM(s.context_edits_applied), 0) AS edits,
                        COUNT(CASE WHEN s.op = 'keep_alive' THEN 1 END) AS ka,
                        SUM(CASE WHEN s.op = 'keep_alive' THEN s.cost END) AS ka_cost
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
                    keep_alive_cost: row.get("ka_cost")?,
                    ..CacheTotals::default()
                })
            },
        )?;

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
        period: Period,
        group_by: GroupBy,
        zone: DayZone,
    ) -> DbResult<Vec<SeriesRow>> {
        let sql = format!(
            "SELECT {day} AS day, {key} AS key, SUM(s.cost) AS cost,
                    SUM(s.input + s.cache_read + s.cache_write) AS pt, SUM(s.output) AS output
               FROM spend s WHERE {filter}
              GROUP BY 1, 2 ORDER BY 1, 2",
            day = zone.sql("s.ts"),
            key = group_key(group_by, zone),
            filter = Self::spend_where(scope, zone),
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
        let currency: Option<String> = self
            .0
            .query_row(
                "SELECT currency FROM spend WHERE chat_id = ?1 AND currency IS NOT NULL
                  ORDER BY ts DESC, id DESC LIMIT 1",
                [id],
                |row| row.get(0),
            )
            .optional()?;

        let (total, cache_read, prompt_total) = self.0.query_row(
            "SELECT SUM(CASE WHEN currency = ?2 THEN cost END) AS total,
                    COALESCE(SUM(CASE WHEN op = 'step' THEN cache_read END), 0) AS cr,
                    COALESCE(SUM(CASE WHEN op = 'step' THEN input + cache_read + cache_write END), 0) AS pt
               FROM spend WHERE chat_id = ?1",
            (id, &currency),
            |row| Ok((row.get("total")?, count(row, "cr")?, count(row, "pt")?)),
        )?;

        let mut stmt = self.0.prepare(
            "SELECT model, SUM(CASE WHEN currency = ?2 THEN cost END) AS cost,
                    SUM(input + cache_read + cache_write) AS pt, SUM(output) AS output
               FROM spend WHERE chat_id = ?1 GROUP BY model ORDER BY model",
        )?;
        let by_model = stmt
            .query_map((id, &currency), |row| {
                Ok(ModelTotals {
                    model: ModelId(row.get("model")?),
                    cost: row.get("cost")?,
                    prompt_total: count(row, "pt")?,
                    output: count(row, "output")?,
                })
            })?
            .collect::<Result<_, _>>()?;

        let last: Option<String> = self
            .0
            .query_row(
                "SELECT turn_id FROM spend WHERE chat_id = ?1 AND turn_id IS NOT NULL
                  ORDER BY ts DESC, id DESC LIMIT 1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        let last_turn = last
            .map(|turn| {
                self.0.query_row(
                    "SELECT SUM(CASE WHEN currency = ?3 THEN cost END) AS cost,
                            SUM(input + cache_read + cache_write) AS pt, SUM(cache_read) AS cr
                       FROM spend WHERE chat_id = ?1 AND turn_id = ?2",
                    (id, &turn, &currency),
                    |row| {
                        Ok(TurnTotals {
                            turn: TurnId(turn.clone()),
                            cost: row.get("cost")?,
                            prompt_total: count(row, "pt")?,
                            cache_read: count(row, "cr")?,
                        })
                    },
                )
            })
            .transpose()?;

        Ok(ChatSpendRow {
            currency,
            total,
            by_model,
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

    /// Незабытые модули; `kind` — живого токена из `module_tokens`.
    pub fn modules(&self) -> DbResult<Vec<ModuleRow>> {
        let mut stmt = self.0.prepare(
            "SELECT m.name, m.state, m.capabilities, m.last_seen_at,
                    (SELECT t.kind FROM module_tokens t
                      WHERE t.module = m.name AND t.revoked_at IS NULL
                      ORDER BY t.issued_at DESC LIMIT 1) AS kind
               FROM modules m WHERE m.state <> 'forgotten' ORDER BY m.name",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ModuleRow {
                    module: ModuleId(row.get("name")?),
                    kind: row.get::<_, Option<Text<TokenKind>>>("kind")?.map(|t| t.0),
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
}

/// Выражение группы `GroupBy` над `spend s`.
fn group_key(group_by: GroupBy, zone: DayZone) -> String {
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
