//! Таблица `soul_meta`: служебные ключи soul (`10 §2.8`). Ключи — константы
//! здесь: вторая строка с тем же смыслом невозможна.

use eva::time::Timestamp;
use rusqlite::{Connection, OptionalExtension as _};

use eva_protocol::types::ChatId;

use crate::db::{DbError, DbResult};

/// Начало последней уборки (`08 §3.1`).
pub const GC_LAST_STARTED: &str = "gc_last_started";
/// Persistent-чат `sleep` (`08 §2.2`).
pub const SLEEP_CHAT_ID: &str = "sleep_chat_id";
/// Граница ночи, пропущенной без строки `sleep_runs` (`08 §2.1`).
pub const SLEEP_SKIPPED_AT: &str = "sleep_skipped_at";
/// Версия overlay групп тулов (`08 §2.4`, `02 §1.3`).
pub const TOOLS_OVERLAY_VERSION: &str = "tools_overlay_version";
/// Последнее уведомление о переполнении core-памяти.
pub const CORE_OVERFLOW_NOTIFIED_AT: &str = "core_overflow_notified_at";

pub struct Meta<'c>(pub &'c Connection);

impl Meta<'_> {
    pub fn get(&self, key: &str) -> DbResult<Option<String>> {
        Ok(self
            .0
            .query_row("SELECT value FROM soul_meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn set(&self, key: &str, value: &str) -> DbResult<()> {
        self.0.execute(
            "INSERT INTO soul_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    /// Неотрицательное целое под ключом; иное — порча строки.
    fn integer(&self, key: &str) -> DbResult<Option<u64>> {
        self.get(key)?
            .map(|raw| {
                raw.parse()
                    .map_err(|e| DbError::corrupt(format_args!("soul_meta.{key} = {raw:?}: {e}")))
            })
            .transpose()
    }

    pub fn timestamp(&self, key: &str) -> DbResult<Option<Timestamp>> {
        Ok(self
            .integer(key)?
            .and_then(std::num::NonZeroU64::new)
            .map(Timestamp::from_nanos))
    }

    pub fn set_timestamp(&self, key: &str, at: Timestamp) -> DbResult<()> {
        self.set(key, &at.as_nanos().to_string())
    }

    pub fn chat(&self, key: &str) -> DbResult<Option<ChatId>> {
        Ok(self
            .integer(key)?
            .map(|n| ChatId(i64::try_from(n).unwrap_or(i64::MAX))))
    }

    pub fn set_chat(&self, key: &str, chat: ChatId) -> DbResult<()> {
        self.set(key, &chat.get().to_string())
    }

    /// `key += 1`, новое значение.
    pub fn increment(&self, key: &str) -> DbResult<u64> {
        let next = self.integer(key)?.unwrap_or(0).saturating_add(1);
        self.set(key, &next.to_string())?;
        Ok(next)
    }
}
