//! Расширение секции `system` фреймворка (`00 §5`): ключи `rt`/`logging`
//! читает сам `eva`, здесь — только свои.

use std::path::PathBuf;

use eva::data;

#[data]
#[serde(default)]
pub struct SystemConfig {
    /// Корень всех путей глав.
    pub state_dir: PathBuf,
    /// Зона календарных суток (`09 §1`): `UTC`/`Etc/UTC`, `±HH:MM` или имя IANA
    /// из встроенной tz-базы; `None` — зона хоста, прочее роняет старт.
    pub tz: Option<String>,
}

impl Default for SystemConfig {
    fn default() -> Self {
        Self {
            state_dir: default_state_dir(),
            tz: None,
        }
    }
}

/// `$STATE_DIRECTORY` systemd (первый из списка через `:`), иначе `/var/lib/eva`.
fn default_state_dir() -> PathBuf {
    std::env::var("STATE_DIRECTORY")
        .ok()
        .and_then(|dirs| dirs.split(':').next().map(str::to_owned))
        .filter(|dir| !dir.is_empty())
        .map_or_else(|| PathBuf::from("/var/lib/eva"), PathBuf::from)
}
