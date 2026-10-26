//! Таблицы `chat_notes`, `notes_state` (`04`, `10 §2.4`).

use eva::{data, time::Timestamp};
use rusqlite::{Connection, OptionalExtension as _, Row, named_params};

use eva_protocol::types::{
    ChatId, ModelId, SegmentId,
    note::{Mode, State},
};

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

#[data]
pub struct SnapshotRow {
    pub id: i64,
    pub created_at: Timestamp,
    pub reason: String,
    pub count: u32,
}

const SELECT: &str = "SELECT name, description, body, prev_body, pinned, created_at, updated_at
  FROM chat_notes";

pub struct Notes<'c>(pub &'c Connection);

impl Notes<'_> {
    /// Полный снимок перед мутацией; вызывается в той же транзакции писателя.
    pub fn checkpoint(
        &self,
        chat: ChatId,
        reason: &str,
        now: Timestamp,
        keep: u32,
    ) -> DbResult<i64> {
        self.0.execute(
            "INSERT INTO notes_snapshots (chat_id, created_at, reason, count)
             SELECT ?1, ?2, ?3, count(*) FROM chat_notes WHERE chat_id = ?1 AND deleted_at IS NULL",
            (chat.get(), Ts(now), reason),
        )?;
        let id = self.0.last_insert_rowid();
        self.0.execute(
            "INSERT INTO notes_snapshot_entries (snapshot_id, name, description, body, prev_body, pinned, created_at, updated_at)
             SELECT ?2, name, description, body, prev_body, pinned, created_at, updated_at
               FROM chat_notes WHERE chat_id = ?1 AND deleted_at IS NULL",
            (chat.get(), id),
        )?;
        self.0.execute(
            "DELETE FROM notes_snapshots WHERE chat_id = ?1 AND id IN
             (SELECT id FROM notes_snapshots WHERE chat_id = ?1 ORDER BY id DESC LIMIT -1 OFFSET ?2)",
            (chat.get(), keep.max(1)),
        )?;
        Ok(id)
    }

    pub fn history(
        &self,
        chat: ChatId,
        before: Option<i64>,
        limit: u32,
    ) -> DbResult<Vec<SnapshotRow>> {
        let mut stmt = self.0.prepare(
            "SELECT id, created_at AS snapshot_created_at, reason, count FROM notes_snapshots
             WHERE chat_id = ?1 AND (?2 IS NULL OR id < ?2) ORDER BY id DESC LIMIT ?3",
        )?;
        Ok(stmt
            .query_map((chat.get(), before, limit), snapshot_row)?
            .collect::<Result<_, _>>()?)
    }

    pub fn snapshot(&self, chat: ChatId, id: i64) -> DbResult<Option<(SnapshotRow, Vec<NoteRow>)>> {
        // Один SELECT держит согласованный снимок даже при параллельной обрезке истории.
        let mut stmt = self.0.prepare(
            "SELECT s.id, s.created_at AS snapshot_created_at, s.reason, s.count,
                    e.snapshot_id AS entry_id, e.name, e.description, e.body, e.prev_body,
                    e.pinned, e.created_at, e.updated_at
               FROM notes_snapshots s LEFT JOIN notes_snapshot_entries e ON e.snapshot_id = s.id
              WHERE s.chat_id = ?1 AND s.id = ?2
              ORDER BY e.pinned DESC, e.created_at ASC, e.name ASC",
        )?;
        let mut rows = stmt.query((chat.get(), id))?;
        let Some(first) = rows.next()? else {
            return Ok(None);
        };
        let snapshot = snapshot_row(first)?;
        let mut notes = Vec::new();
        if first.get::<_, Option<i64>>("entry_id")?.is_some() {
            notes.push(from_row(first)?);
        }
        while let Some(row) = rows.next()? {
            notes.push(from_row(row)?);
        }
        Ok(Some((snapshot, notes)))
    }

    /// Замена блокнота уже проверенным снимком; снимок текущего сделал вызывающий.
    pub fn replace(&self, chat: ChatId, notes: &[NoteRow], now: Timestamp) -> DbResult<()> {
        self.0.execute(
            "UPDATE chat_notes SET deleted_at = ?2 WHERE chat_id = ?1 AND deleted_at IS NULL",
            (chat.get(), Ts(now)),
        )?;
        for note in notes {
            self.insert(chat, note)?;
        }
        Ok(())
    }

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
        let sql = format!(
            "{SELECT} WHERE chat_id = ?1 AND deleted_at IS NULL
              ORDER BY pinned DESC, created_at ASC, name ASC"
        );
        let mut stmt = self.0.prepare(&sql)?;
        let rows = stmt
            .query_map([chat.get()], from_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Живая заметка по имени.
    pub fn get(&self, chat: ChatId, name: &str) -> DbResult<Option<NoteRow>> {
        let sql = format!("{SELECT} WHERE chat_id = ?1 AND name = ?2 AND deleted_at IS NULL");
        Ok(self
            .0
            .query_row(&sql, (chat.get(), name), from_row)
            .optional()?)
    }

    /// Новая заметка; удалённая под тем же именем оживает как новая (`04 §7`).
    pub fn insert(&self, chat: ChatId, note: &NoteRow) -> DbResult<()> {
        self.0.execute(
            "INSERT INTO chat_notes (chat_id, name, description, body, prev_body, pinned, created_at, updated_at, deleted_at)
             VALUES (:chat, :name, :description, :body, :prev_body, :pinned, :created_at, :updated_at, NULL)
             ON CONFLICT (chat_id, name) DO UPDATE SET description = excluded.description, body = excluded.body,
                prev_body = excluded.prev_body, pinned = excluded.pinned, created_at = excluded.created_at,
                updated_at = excluded.updated_at, deleted_at = NULL",
            named_params! {
                ":chat": chat.get(),
                ":name": note.name,
                ":description": note.description,
                ":body": note.body,
                ":prev_body": note.prev_body,
                ":pinned": note.pinned,
                ":created_at": Ts(note.created_at),
                ":updated_at": Ts(note.updated_at),
            },
        )?;
        Ok(())
    }

    /// Перезапись живой заметки целиком; `prev_body` решает вызывающий.
    pub fn save(&self, chat: ChatId, note: &NoteRow) -> DbResult<()> {
        self.0.execute(
            "UPDATE chat_notes SET description = :description, body = :body, prev_body = :prev_body,
                    pinned = :pinned, updated_at = :updated_at
              WHERE chat_id = :chat AND name = :name AND deleted_at IS NULL",
            named_params! {
                ":chat": chat.get(),
                ":name": note.name,
                ":description": note.description,
                ":body": note.body,
                ":prev_body": note.prev_body,
                ":pinned": note.pinned,
                ":updated_at": Ts(note.updated_at),
            },
        )?;
        Ok(())
    }

    /// Soft-delete; `false` — живой заметки с таким именем нет.
    pub fn delete(&self, chat: ChatId, name: &str, now: Timestamp) -> DbResult<bool> {
        let changed = self.0.execute(
            "UPDATE chat_notes SET deleted_at = ?3 WHERE chat_id = ?1 AND name = ?2 AND deleted_at IS NULL",
            (chat.get(), name, Ts(now)),
        )?;
        Ok(changed > 0)
    }

    /// Строка `notes_state`; нет до первого тура с блокнотом.
    pub fn state(&self, chat: ChatId) -> DbResult<Option<State>> {
        Ok(self
            .0
            .query_row(
                "SELECT mode, since_at, squeeze_turns, wrap_up_segment, nudge_segment, seen_at
                   FROM notes_state WHERE chat_id = ?1",
                [chat.get()],
                |row| {
                    Ok(State {
                        mode: row.get::<_, Text<Mode>>("mode")?.0,
                        since_at: row.get::<_, Ts>("since_at")?.0,
                        squeeze_turns: row.get("squeeze_turns")?,
                        wrap_up_segment: row
                            .get::<_, Option<i64>>("wrap_up_segment")?
                            .map(SegmentId),
                        nudge_segment: row.get::<_, Option<i64>>("nudge_segment")?.map(SegmentId),
                        seen_at: row.get::<_, Option<Ts>>("seen_at")?.map(|t| t.0),
                    })
                },
            )
            .optional()?)
    }

    pub fn save_state(&self, chat: ChatId, state: &State) -> DbResult<()> {
        self.0.execute(
            "INSERT INTO notes_state (chat_id, mode, since_at, squeeze_turns, wrap_up_segment, nudge_segment, seen_at)
             VALUES (:chat, :mode, :since_at, :squeeze_turns, :wrap_up, :nudge, :seen_at)
             ON CONFLICT (chat_id) DO UPDATE SET mode = excluded.mode, since_at = excluded.since_at,
                squeeze_turns = excluded.squeeze_turns, wrap_up_segment = excluded.wrap_up_segment,
                nudge_segment = excluded.nudge_segment, seen_at = excluded.seen_at",
            named_params! {
                ":chat": chat.get(),
                ":mode": Text(state.mode),
                ":since_at": Ts(state.since_at),
                ":squeeze_turns": state.squeeze_turns,
                ":wrap_up": state.wrap_up_segment.map(SegmentId::get),
                ":nudge": state.nudge_segment.map(SegmentId::get),
                ":seen_at": state.seen_at.map(Ts),
            },
        )?;
        Ok(())
    }
}

fn snapshot_row(row: &Row<'_>) -> rusqlite::Result<SnapshotRow> {
    Ok(SnapshotRow {
        id: row.get("id")?,
        created_at: row.get::<_, Ts>("snapshot_created_at")?.0,
        reason: row.get("reason")?,
        count: row.get("count")?,
    })
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<NoteRow> {
    Ok(NoteRow {
        name: row.get("name")?,
        description: row.get("description")?,
        body: row.get("body")?,
        prev_body: row.get("prev_body")?,
        pinned: row.get("pinned")?,
        created_at: row.get::<_, Ts>("created_at")?.0,
        updated_at: row.get::<_, Ts>("updated_at")?.0,
    })
}
