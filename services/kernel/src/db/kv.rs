//! Таблица `module_kv`: KV модулей (`06 §5`, `10 §2.6`). Значение — msgpack.

use eva::time::Timestamp;
use rusqlite::{Connection, OptionalExtension as _, named_params};

use eva_protocol::types::ModuleId;

use crate::db::{DbResult, sql::Ts};

/// Цена строки сверх байтов ключа и значения: заголовок записи и индекс.
/// Та же константа — в триггерах `module_kv_usage` (миграция `0002`).
pub const ROW_OVERHEAD: u64 = 64;

/// Сколько строка весит в квоте `modules.kv_quota`.
pub fn row_cost(key: &str, value: &[u8]) -> u64 {
    (key.len() as u64)
        .saturating_add(value.len() as u64)
        .saturating_add(ROW_OVERHEAD)
}

/// Вес модуля `?1` без строки `?2`: две выборки по ключу, без обхода ключей модуля.
pub const USAGE: &str = "SELECT COALESCE((SELECT bytes FROM module_kv_usage WHERE module = ?1), 0)
          - COALESCE((SELECT octet_length(key) + length(value) + ?3
                      FROM module_kv WHERE module = ?1 AND key = ?2), 0)";

pub struct Kv<'c>(pub &'c Connection);

impl Kv<'_> {
    /// Вес строк модуля в квоте, кроме `except`: её значение сейчас заменят.
    pub fn usage(&self, module: &ModuleId, except: &str) -> DbResult<u64> {
        let used: i64 = self.0.query_row(
            USAGE,
            (module.as_str(), except, ROW_OVERHEAD as i64),
            |row| row.get(0),
        )?;
        Ok(u64::try_from(used).unwrap_or(0))
    }

    pub fn get(&self, module: &ModuleId, key: &str) -> DbResult<Option<Vec<u8>>> {
        Ok(self
            .0
            .query_row(
                "SELECT value FROM module_kv WHERE module = ?1 AND key = ?2",
                (module.as_str(), key),
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Last-write-wins.
    pub fn set(&self, module: &ModuleId, key: &str, value: &[u8], now: Timestamp) -> DbResult<()> {
        self.0.execute(
            "INSERT INTO module_kv (module, key, value, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (module, key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            (module.as_str(), key, value, Ts(now)),
        )?;
        Ok(())
    }

    pub fn del(&self, module: &ModuleId, key: &str) -> DbResult<()> {
        self.0.execute(
            "DELETE FROM module_kv WHERE module = ?1 AND key = ?2",
            (module.as_str(), key),
        )?;
        Ok(())
    }

    /// Ключи с префиксом по возрастанию, keyset по ключу.
    pub fn list(
        &self,
        module: &ModuleId,
        prefix: &str,
        after: Option<&str>,
        limit: u16,
    ) -> DbResult<Vec<String>> {
        // `substr` вместо `LIKE`: в префиксе модуля `%` и `_` — обычные символы.
        let mut stmt = self.0.prepare(
            "SELECT key FROM module_kv
             WHERE module = :module AND key >= :prefix
               AND substr(key, 1, length(:prefix)) = :prefix
               AND (:after IS NULL OR key > :after)
             ORDER BY key LIMIT :limit",
        )?;
        let keys = stmt
            .query_map(
                named_params! {
                    ":module": module.as_str(),
                    ":prefix": prefix,
                    ":after": after,
                    ":limit": limit,
                },
                |row| row.get(0),
            )?
            .collect::<Result<_, _>>()?;
        Ok(keys)
    }
}
