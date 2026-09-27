//! Транскрипт: `segments`, `events`, `messages`, `tool_results`, `token_ratios` (`01`, `10 §2.1`).

use rusqlite::{Connection, OptionalExtension as _};

use eva_protocol::types::ModelId;

use crate::db::DbResult;

pub struct Turns<'c>(pub &'c Connection);

impl Turns<'_> {
    /// Калиброванный `bytes_per_token` модели (`01 §9`); `None` — модель ещё не калибрована.
    pub fn ratio(&self, model: &ModelId) -> DbResult<Option<f64>> {
        Ok(self
            .0
            .query_row(
                "SELECT bytes_per_token FROM token_ratios WHERE model = ?1",
                [model.as_str()],
                |row| row.get(0),
            )
            .optional()?)
    }
}
