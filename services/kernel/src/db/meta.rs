//! Таблица `kernel_meta`: служебные ключи ядра (`10 §2.8`).

use rusqlite::{Connection, OptionalExtension as _};

use crate::db::DbResult;

pub struct Meta<'c>(pub &'c Connection);

impl Meta<'_> {
    pub fn get(&self, key: &str) -> DbResult<Option<String>> {
        Ok(self
            .0
            .query_row(
                "SELECT value FROM kernel_meta WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn set(&self, key: &str, value: &str) -> DbResult<()> {
        self.0.execute(
            "INSERT INTO kernel_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }
}
