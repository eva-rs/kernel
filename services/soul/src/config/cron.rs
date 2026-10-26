//! Секция `cron` (`08 §1.5`).

use std::time::Duration;

use eva::{data, time::HumanDuration};

#[data]
#[serde(default)]
pub struct CronConfig {
    pub tick: HumanDuration,
    /// Токенов `cron_runs.output`.
    pub output_max: u32,
}

impl Default for CronConfig {
    fn default() -> Self {
        Self {
            tick: HumanDuration(Duration::from_secs(5)),
            output_max: 2000,
        }
    }
}
