//! Таблицы `chat_notes`, `notes_state` (`04`, `10 §2.4`).

use eva::{data, time::Timestamp};
use rusqlite::{Connection, OptionalExtension as _};

use eva_protocol::types::{ChatId, ModelId, note::Mode};

use crate::db::{
    DbResult,
    sql::{Text, Ts},
};

#[data]
pub struct NoteRow {
    pub name: String,
    pub description: String,
    pub body: String,
    pub prev_body: Option<String>,
    pub pinned: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

pub struct Notes<'c>(pub &'c Connection);

impl Notes<'_> {
    /// Эффективная модель чата `chat ?? folder ?? surface` (`01 §8`); `None` — чата нет,
    /// `Some(None)` — цепочка пуста.
    pub fn chat_model(&self, chat: ChatId) -> DbResult<Option<Option<ModelId>>> {
        Ok(self
            .0
            .query_row(
                "SELECT COALESCE(c.model, f.model, s.model)
                   FROM chats c JOIN folders f ON f.id = c.folder_id JOIN surfaces s ON s.id = f.surface_id
                  WHERE c.id = ?1",
                [chat.get()],
                |row| row.get::<_, Option<String>>(0).map(|m| m.map(ModelId)),
            )
            .optional()?)
    }

    /// Живые заметки в порядке секции (`04 §1`).
    pub fn list(&self, chat: ChatId) -> DbResult<Vec<NoteRow>> {
        let mut stmt = self.0.prepare(
            "SELECT name, description, body, prev_body, pinned, created_at, updated_at
               FROM chat_notes WHERE chat_id = ?1 AND deleted_at IS NULL
              ORDER BY pinned DESC, created_at ASC, name ASC",
        )?;
        let rows = stmt
            .query_map([chat.get()], |row| {
                Ok(NoteRow {
                    name: row.get("name")?,
                    description: row.get("description")?,
                    body: row.get("body")?,
                    prev_body: row.get("prev_body")?,
                    pinned: row.get("pinned")?,
                    created_at: row.get::<_, Ts>("created_at")?.0,
                    updated_at: row.get::<_, Ts>("updated_at")?.0,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Режим из `notes_state`; строки нет до первой заметки.
    pub fn mode(&self, chat: ChatId) -> DbResult<Option<Mode>> {
        Ok(self
            .0
            .query_row(
                "SELECT mode FROM notes_state WHERE chat_id = ?1",
                [chat.get()],
                |row| row.get::<_, Text<Mode>>(0).map(|t| t.0),
            )
            .optional()?)
    }
}
