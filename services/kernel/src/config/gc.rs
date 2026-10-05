//! Секция `gc` (`08 §3.4`): ретеншны — конфиг, не константы в SQL.

use std::time::Duration;

use eva::{data, time::HumanDuration};

use crate::config::wall::{WallTime, Weekday, WeeklyAt};

#[data]
#[serde(default)]
pub struct GcConfig {
    pub tick: HumanDuration,
    /// Еженедельный слот уборки в `system.tz`.
    pub at: WeeklyAt,
    pub ephemeral_idle: HumanDuration,
    pub deleted_after: HumanDuration,
    /// Каталог blob'а без строки старше этого — сирота.
    pub blob_orphan_after: HumanDuration,
    /// Забытая память и удалённые заметки.
    pub memory_archive_after: HumanDuration,
    pub embed_cache_after: HumanDuration,
    pub pending_after: HumanDuration,
    pub cron_runs_keep: u32,
    pub sleep_runs_keep: u32,
    pub gc_runs_keep: u32,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            tick: mins(5),
            at: WeeklyAt {
                weekday: Weekday::Sun,
                time: WallTime("03:00".parse().unwrap_or_else(|_| unreachable!())),
            },
            ephemeral_idle: days(2),
            deleted_after: days(365),
            blob_orphan_after: days(1),
            memory_archive_after: days(30),
            embed_cache_after: days(90),
            pending_after: days(30),
            cron_runs_keep: 500,
            sleep_runs_keep: 200,
            gc_runs_keep: 52,
        }
    }
}

const fn mins(n: u64) -> HumanDuration {
    HumanDuration(Duration::from_secs(n * 60))
}

pub(crate) const fn days(n: u64) -> HumanDuration {
    HumanDuration(Duration::from_secs(n * 24 * 60 * 60))
}
