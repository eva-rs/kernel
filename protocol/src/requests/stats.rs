//! Статистика: только чтение.

use eva::{data, time::Date};

use crate::{
    errors,
    types::{
        ChatId,
        stats::{
            CacheStats, Counter, Gauges, GroupBy, ModelSpend, Money, RateLimit, Scope, SeriesPoint,
            SpendRow, Target, ToolUsageRow, TurnSpend,
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

pub mod tools_usage {
    use super::*;

    #[data]
    pub struct Args {
        #[serde(default)]
        pub scope: Scope,
        /// `None` — только при `Scope::Global`.
        pub target: Option<Target>,
        /// Календарные сутки в `system.tz`, включительно.
        pub from: Date,
        pub to: Date,
        #[serde(default)]
        pub top: Top,
        pub view: Option<TurnView>,
    }

    #[data]
    pub struct Ok {
        pub rows: Vec<ToolUsageRow>,
        /// Знаменатель покрытия.
        pub turns: u64,
    }

    #[data(error, display("_"))]
    pub enum Err {}

    code!(Err => []);
}

pub mod spend {
    use super::*;

    #[data]
    pub struct Args {
        #[serde(default)]
        pub scope: Scope,
        pub target: Option<Target>,
        pub from: Date,
        pub to: Date,
        #[serde(default)]
        pub group_by: GroupBy,
        pub view: Option<TurnView>,
    }

    #[data]
    pub struct Ok {
        pub rows: Vec<SpendRow>,
        pub cache: CacheStats,
    }

    #[data(error, display("_"))]
    pub enum Err {}

    code!(Err => []);
}

pub mod spend_series {
    use super::*;

    #[data]
    pub struct Args {
        #[serde(default)]
        pub scope: Scope,
        pub target: Option<Target>,
        pub from: Date,
        pub to: Date,
        #[serde(default)]
        pub group_by: GroupBy,
    }

    #[data]
    pub struct Ok {
        pub series: Vec<SeriesPoint>,
    }

    #[data(error, display("_"))]
    pub enum Err {}

    code!(Err => []);
}

pub mod chat_spend {
    use super::*;

    #[data]
    pub struct Args {
        pub chat: ChatId,
    }

    #[data]
    pub struct Ok {
        /// `None` — ни одной оценённой строки.
        pub total: Option<Money>,
        pub by_model: Vec<ModelSpend>,
        pub last_turn: Option<TurnSpend>,
        pub hit_ratio: f64,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::chats::NotFound),
    }

    code!(Err => [NotFound]);
}

pub mod health {
    use super::*;

    #[data]
    pub struct Args {
        pub from: Date,
        pub to: Date,
    }

    #[data]
    pub struct Ok {
        pub gauges: Gauges,
        /// Только ненулевые.
        pub counters: Vec<Counter>,
        pub rate_limits: Vec<RateLimit>,
    }

    #[data(error, display("_"))]
    pub enum Err {}

    code!(Err => []);
}
