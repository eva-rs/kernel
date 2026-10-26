//! Секция `db` (`10 §7`).

use std::{path::PathBuf, time::Duration};

use eva::{data, time::HumanDuration};

/// Потолок читающих соединений: сверх него запрос ждёт свободное до `busy_timeout`.
#[eva::int(u8, 1..=64)]
pub enum Readers {}

#[data]
#[serde(default)]
pub struct DbConfig {
    /// `None` — `<state_dir>/eva.sqlite`; `:memory:` — база в памяти (тесты).
    pub path: Option<PathBuf>,
    pub busy_timeout: HumanDuration,
    pub readers: Readers,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            path: None,
            busy_timeout: HumanDuration(Duration::from_secs(5)),
            readers: Readers::POS4,
        }
    }
}
