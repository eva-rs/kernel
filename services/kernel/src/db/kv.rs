//! Таблица `module_kv`: KV модулей (`06 §5`, `10 §2.6`). Значение — msgpack.

use eva::time::Timestamp;
use rusqlite::{Connection, OptionalExtension as _, named_params};

use eva_protocol::types::ModuleId;

use crate::db::{DbResult, sql::Ts};

pub struct Kv<'c>(pub &'c Connection);

impl Kv<'_> {
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
