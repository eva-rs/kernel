//! Статистика: только чтение.

use crate::requests::Response;
use eva::{service, time::Date};

use crate::{
    errors,
    types::{
        ChatId,
        stats::{
            CacheStats, Counter, Gauges, GroupBy, ModelSpend, Money, RateLimit, Scope, SeriesPoint,
            SpendRow, SpendWindow, Target, ToolUsageRow, TurnSpend,
        },
        turn::TurnView,
    },
};

/// Сколько тулов показать.
#[eva::int(u8, 1..=100)]
pub enum Top {}

impl Default for Top {
    fn default() -> Self {
        Self::POS20
    }
}

#[service(result = Response)]
pub trait Stats {
    fn tools_usage(
        #[serde(default)] scope: Scope,
        /// `None` — только при `Scope::Global`.
        target: Option<Target>,
        /// Календарные сутки в `system.tz`, включительно.
        from: Date,
        to: Date,
        #[serde(default)] top: Top,
        view: Option<TurnView>,
    ) -> ok! {
        rows: Vec<ToolUsageRow>,
        /// Знаменатель покрытия.
        turns: u64,
    };
    embed! {
        code!(Err => []);
    }

    fn spend(
        #[serde(default)] scope: Scope,
        target: Option<Target>,
        window: SpendWindow,
        /// Зона календарных границ и группировки по суткам; `None` — `system.tz`.
        timezone: Option<String>,
        #[serde(default)] group_by: GroupBy,
        view: Option<TurnView>,
    ) -> ok! {
        rows: Vec<SpendRow>,
        cache: CacheStats,
    };
    embed! {
        code!(Err => []);
    }

    fn spend_series(
        #[serde(default)] scope: Scope,
        target: Option<Target>,
        window: SpendWindow,
        /// Зона календарных границ и суточных точек; `None` — `system.tz`.
        timezone: Option<String>,
        #[serde(default)] group_by: GroupBy,
    ) -> ok! {
        series: Vec<SeriesPoint>,
    };
    embed! {
        code!(Err => []);
    }

    #[errors(errors::chats::NotFound)]
    fn chat_spend(
        chat: ChatId,
    ) -> ok! {
        /// Итоги по валютам; пусто — ни одной оценённой строки.
        total: Vec<Money>,
        by_model: Vec<ModelSpend>,
        last_turn: Option<TurnSpend>,
        hit_ratio: f64,
    };
    embed! {
        code!(Err => [NotFound]);
    }

    fn health(
        from: Date,
        to: Date,
    ) -> ok! {
        gauges: Gauges,
        /// Только ненулевые.
        counters: Vec<Counter>,
        rate_limits: Vec<RateLimit>,
    };
    embed! {
        code!(Err => []);
    }
}
