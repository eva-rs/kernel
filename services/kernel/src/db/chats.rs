//! Таблицы `chats`, `chat_aliases`, `chat_speech` (+fts, `chat_speech_vec`) (`05`, `10 §2.5`).

use eva::{data, time::Timestamp};
use rusqlite::{Connection, OptionalExtension as _, Row, named_params};

use eva_protocol::types::{
    ChatId, Effort, EventId, FolderId, ModelId, ModuleId, PersonId, SurfaceId, Thinking, TurnId,
    chat::{Alias, ExternalKind, Kind, Role},
    content::Block,
};

use crate::db::{
    DbResult,
    sql::{Level, Text, Ts},
    surfaces::{SurfaceFilter, SurfaceRef},
};

/// Чат для `Policy`: права решает его surface.
#[data]
pub struct ChatRef {
    pub id: ChatId,
    pub surface: SurfaceRef,
}

/// Строка `chats` с тем, что к ней вычисляет чтение: alias, surface, trust, живой тур.
#[data]
pub struct ChatRow {
    pub id: ChatId,
    pub folder_id: FolderId,
    pub surface: SurfaceRef,
    pub kind: Kind,
    pub title: Option<String>,
    pub external_title: Option<String>,
    pub alias: Option<Alias>,
    pub pinned: bool,
    pub alerted: bool,
    pub undeletable: bool,
    pub deleted_at: Option<Timestamp>,
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
    pub thinking: Option<Thinking>,
    pub compact_at: Option<f32>,
    pub trusted: Option<bool>,
    /// `chat.trusted ?? folder.trusted ?? surface.trusted ?? false` (`05 §2.2`).
    pub trusted_chat: bool,
    pub project: Option<String>,
    pub workdir: Option<String>,
    pub workdir_on: Option<String>,
    pub live_turn: Option<TurnId>,
    /// Чат подагента: его trust — снимок тимлида (`05 §2.2`).
    pub agent: bool,
    pub created_at: Timestamp,
    pub last_activity_at: Timestamp,
}

impl ChatRow {
    pub fn chat_ref(&self) -> ChatRef {
        ChatRef {
            id: self.id,
            surface: self.surface.clone(),
        }
    }
}

/// Новый чат `chats.create`.
#[data]
pub struct NewChat {
    pub folder_id: FolderId,
    pub kind: Kind,
    pub title: Option<String>,
    pub project: Option<String>,
    pub workdir: Option<String>,
    pub workdir_on: Option<String>,
    pub now: Timestamp,
}

/// Фильтр `chats.list`; видимость caller'а — `surfaces`.
#[data]
pub struct ChatFilter {
    pub surfaces: SurfaceFilter,
    pub surface: Option<SurfaceId>,
    pub folder: Option<FolderId>,
    pub deleted: bool,
    pub live: bool,
}

/// Реплика истории: изменяемая сторона из `messages`, блоки — из `events.payload`.
#[data]
pub struct MessageRow {
    pub event_id: EventId,
    pub seq: u64,
    pub role: Role,
    pub sender: Option<PersonId>,
    pub blocks: Vec<Block>,
    pub external_id: Option<String>,
    pub reply_to_external_id: Option<String>,
    pub turn: Option<TurnId>,
    pub created_at: Timestamp,
}

/// Часть payload события `message` (`01 §1.2`), которую читает история.
#[data]
struct MessagePayload {
    blocks: Vec<Block>,
}

/// Открытый тур — события с `turn_id` без `turn_ended` (`01 §1.1`); тур живёт в открытом сегменте.
const SELECT: &str = "SELECT c.id, c.folder_id, f.surface_id, s.module AS surface_module, c.kind,
       c.title, c.external_title, a.external_id AS alias_id, a.external_kind AS alias_kind,
       c.pinned, c.alerted, c.undeletable, c.deleted_at, c.model, c.effort, c.thinking,
       c.compact_at, c.trusted, COALESCE(c.trusted, f.trusted, s.trusted, 0) AS trusted_chat,
       c.project, c.workdir, c.workdir_on, c.created_at, c.last_activity_at,
       (SELECT e.turn_id FROM segments g JOIN events e ON e.segment_id = g.id
         WHERE g.chat_id = c.id AND g.ended_at IS NULL AND e.turn_id IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM events x WHERE x.turn_id = e.turn_id AND x.kind = 'turn_ended')
         ORDER BY e.seq DESC LIMIT 1) AS live_turn,
       EXISTS (SELECT 1 FROM agents WHERE agents.id = c.id) AS agent
  FROM chats c
  JOIN folders f ON f.id = c.folder_id
  JOIN surfaces s ON s.id = f.surface_id
  LEFT JOIN chat_aliases a ON a.chat_id = c.id";

pub struct Chats<'c>(pub &'c Connection);

impl Chats<'_> {
    pub fn get(&self, id: ChatId) -> DbResult<Option<ChatRow>> {
        let sql = format!("{SELECT} WHERE c.id = ?1");
        Ok(self.0.query_row(&sql, [id.0], from_row).optional()?)
    }

    pub fn by_alias(&self, surface: &SurfaceId, external_id: &str) -> DbResult<Option<ChatRow>> {
        let sql = format!("{SELECT} WHERE a.surface_id = ?1 AND a.external_id = ?2");
        Ok(self
            .0
            .query_row(&sql, (surface.as_str(), external_id), from_row)
            .optional()?)
    }

    /// Только то, что нужно `Policy`: для операций других доменов над чатом.
    pub fn chat_ref(&self, id: ChatId) -> DbResult<Option<ChatRef>> {
        Ok(self
            .0
            .query_row(
                "SELECT c.id, f.surface_id, s.module FROM chats c
                   JOIN folders f ON f.id = c.folder_id
                   JOIN surfaces s ON s.id = f.surface_id
                  WHERE c.id = ?1",
                [id.0],
                |row| {
                    Ok(ChatRef {
                        id: ChatId(row.get(0)?),
                        surface: SurfaceRef {
                            id: SurfaceId(row.get(1)?),
                            module: row.get::<_, Option<String>>(2)?.map(ModuleId),
                        },
                    })
                },
            )
            .optional()?)
    }

    /// Keyset по `(last_activity_at, id) DESC`; ключ `after` читается из БД.
    /// `None` — якорь страницы исчез (purge между страницами).
    pub fn list(
        &self,
        filter: &ChatFilter,
        after: Option<ChatId>,
        limit: u16,
    ) -> DbResult<Option<Vec<ChatRow>>> {
        let after_at = match after {
            Some(id) => match self.last_activity(id)? {
                Some(at) => Some(at),
                None => return Ok(None),
            },
            None => None,
        };
        let sql = format!(
            "SELECT * FROM ({SELECT}) c
             WHERE (:module IS NULL OR c.surface_module = :module)
               AND (:surface IS NULL OR c.surface_id = :surface)
               AND (:folder IS NULL OR c.folder_id = :folder)
               AND ((c.deleted_at IS NOT NULL) = :deleted)
               AND (NOT :live OR c.live_turn IS NOT NULL)
               AND (:after IS NULL OR (c.last_activity_at, c.id) < (:after_at, :after))
             ORDER BY c.last_activity_at DESC, c.id DESC LIMIT :limit"
        );
        let mut stmt = self.0.prepare(&sql)?;
        let rows = stmt
            .query_map(
                named_params! {
                    ":module": filter.surfaces.module(),
                    ":surface": filter.surface.as_ref().map(SurfaceId::as_str),
                    ":folder": filter.folder.map(|f| f.0),
                    ":deleted": filter.deleted,
                    ":live": filter.live,
                    ":after": after.map(|a| a.0),
                    ":after_at": after_at.map(Ts),
                    ":limit": limit,
                },
                from_row,
            )?
            .collect::<Result<_, _>>()?;
        Ok(Some(rows))
    }

    fn last_activity(&self, id: ChatId) -> DbResult<Option<Timestamp>> {
        Ok(self
            .0
            .query_row(
                "SELECT last_activity_at FROM chats WHERE id = ?1",
                [id.0],
                |row| row.get::<_, Ts>(0),
            )
            .optional()?
            .map(|ts| ts.0))
    }

    pub fn insert(&self, chat: &NewChat) -> DbResult<ChatId> {
        let id = self.0.query_row(
            "INSERT INTO chats (folder_id, kind, title, project, workdir, workdir_on, created_at, last_activity_at)
             VALUES (:folder, :kind, :title, :project, :workdir, :workdir_on, :now, :now)
             RETURNING id",
            named_params! {
                ":folder": chat.folder_id.0,
                ":kind": Text(chat.kind),
                ":title": chat.title,
                ":project": chat.project,
                ":workdir": chat.workdir,
                ":workdir_on": chat.workdir_on,
                ":now": Ts(chat.now),
            },
            |row| row.get(0),
        )?;
        Ok(ChatId(id))
    }

    pub fn alias_owner(&self, surface: &SurfaceId, external_id: &str) -> DbResult<Option<ChatId>> {
        Ok(self
            .0
            .query_row(
                "SELECT chat_id FROM chat_aliases WHERE surface_id = ?1 AND external_id = ?2",
                (surface.as_str(), external_id),
                |row| row.get(0).map(ChatId),
            )
            .optional()?)
    }

    pub fn insert_alias(
        &self,
        chat: ChatId,
        surface: &SurfaceId,
        external_id: &str,
        kind: ExternalKind,
        now: Timestamp,
    ) -> DbResult<()> {
        self.0.execute(
            "INSERT INTO chat_aliases (surface_id, external_id, chat_id, external_kind, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (surface.as_str(), external_id, chat.0, Text(kind), Ts(now)),
        )?;
        Ok(())
    }

    /// Изменяемые хозяином и модулем колонки строки.
    pub fn save(&self, row: &ChatRow) -> DbResult<()> {
        self.0.execute(
            "UPDATE chats
             SET kind = :kind, title = :title, pinned = :pinned, alerted = :alerted,
                 model = :model, effort = :effort, thinking = :thinking, compact_at = :compact_at,
                 trusted = :trusted, project = :project, workdir = :workdir, workdir_on = :workdir_on
             WHERE id = :id",
            named_params! {
                ":id": row.id.0,
                ":kind": Text(row.kind),
                ":title": row.title,
                ":pinned": row.pinned,
                ":alerted": row.alerted,
                ":model": row.model.as_ref().map(ModelId::as_str),
                ":effort": row.effort.map(Level),
                ":thinking": row.thinking.map(Text),
                ":compact_at": row.compact_at,
                ":trusted": row.trusted,
                ":project": row.project,
                ":workdir": row.workdir,
                ":workdir_on": row.workdir_on,
            },
        )?;
        Ok(())
    }

    /// Строка `title` индекса поиска — одна на чат, заменяется при переименовании (`05 §7.1`).
    pub fn index_title(&self, chat: ChatId, title: Option<&str>) -> DbResult<()> {
        self.0.execute(
            "DELETE FROM chat_speech WHERE chat_id = ?1 AND kind = 'title'",
            [chat.0],
        )?;
        if let Some(title) = title {
            self.0.execute(
                "INSERT INTO chat_speech (chat_id, seq, chunk_no, kind, text) VALUES (?1, NULL, 0, 'title', ?2)",
                (chat.0, title),
            )?;
        }
        Ok(())
    }

    pub fn move_to(&self, chat: ChatId, folder: FolderId) -> DbResult<()> {
        self.0.execute(
            "UPDATE chats SET folder_id = ?2 WHERE id = ?1",
            (chat.0, folder.0),
        )?;
        Ok(())
    }

    /// Открытый сегмент есть: его закрывает `turns` компакцией `reset` (`05 §2.3`).
    pub fn mark_reset(&self, chat: ChatId) -> DbResult<bool> {
        let changed = self.0.execute(
            "UPDATE chats SET reset_pending = 'reset'
             WHERE id = ?1 AND EXISTS (SELECT 1 FROM segments WHERE chat_id = ?1 AND ended_at IS NULL)",
            [chat.0],
        )?;
        Ok(changed > 0)
    }

    pub fn set_deleted(&self, chat: ChatId, at: Option<Timestamp>) -> DbResult<()> {
        self.0.execute(
            "UPDATE chats SET deleted_at = ?2 WHERE id = ?1",
            (chat.0, at.map(Ts)),
        )?;
        Ok(())
    }

    /// Purge: чат и чаты его подагентов, которых каскад `agents` не уносит (`05 §6`).
    pub fn purge_set(&self, chat: ChatId) -> DbResult<Vec<ChatRef>> {
        let mut stmt = self.0.prepare(
            "SELECT c.id, f.surface_id, s.module FROM chats c
               JOIN folders f ON f.id = c.folder_id
               JOIN surfaces s ON s.id = f.surface_id
              WHERE c.id = ?1 OR c.id IN (SELECT id FROM agents WHERE parent_chat_id = ?1)
              ORDER BY c.id",
        )?;
        let rows = stmt
            .query_map([chat.0], |row| {
                Ok(ChatRef {
                    id: ChatId(row.get(0)?),
                    surface: SurfaceRef {
                        id: SurfaceId(row.get(1)?),
                        module: row.get::<_, Option<String>>(2)?.map(ModuleId),
                    },
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Каскад уносит лог, сегменты, заметки, alias, индекс, строки `agents` (`10 §6`).
    pub fn purge(&self, ids: &[ChatId]) -> DbResult<()> {
        let mut stmt = self.0.prepare("DELETE FROM chats WHERE id = ?1")?;
        for id in ids {
            stmt.execute([id.0])?;
        }
        Ok(())
    }

    /// Реплики с `seq ≥ from_seq` по возрастанию.
    pub fn messages(&self, chat: ChatId, from_seq: u64, count: u16) -> DbResult<Vec<MessageRow>> {
        let mut stmt = self.0.prepare(
            "SELECT m.event_id, m.seq, m.role, m.sender_person_id, m.external_id,
                    m.reply_to_external_id, m.turn_id, e.payload, e.created_at
               FROM messages m JOIN events e ON e.id = m.event_id
              WHERE m.chat_id = ?1 AND m.seq >= ?2
              ORDER BY m.seq LIMIT ?3",
        )?;
        let from = i64::try_from(from_seq).unwrap_or(i64::MAX);
        let rows = stmt
            .query_map((chat.0, from, count), message_from_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<ChatRow> {
    let alias = match row.get::<_, Option<String>>("alias_id")? {
        Some(external_id) => Some(Alias {
            external_id,
            kind: row.get::<_, Text<ExternalKind>>("alias_kind")?.0,
        }),
        None => None,
    };

    Ok(ChatRow {
        id: ChatId(row.get("id")?),
        folder_id: FolderId(row.get("folder_id")?),
        surface: SurfaceRef {
            id: SurfaceId(row.get("surface_id")?),
            module: row
                .get::<_, Option<String>>("surface_module")?
                .map(ModuleId),
        },
        kind: row.get::<_, Text<Kind>>("kind")?.0,
        title: row.get("title")?,
        external_title: row.get("external_title")?,
        alias,
        pinned: row.get("pinned")?,
        alerted: row.get("alerted")?,
        undeletable: row.get("undeletable")?,
        deleted_at: row.get::<_, Option<Ts>>("deleted_at")?.map(|t| t.0),
        model: row.get::<_, Option<String>>("model")?.map(ModelId),
        effort: row.get::<_, Option<Level>>("effort")?.map(|l| l.0),
        thinking: row
            .get::<_, Option<Text<Thinking>>>("thinking")?
            .map(|t| t.0),
        compact_at: row.get("compact_at")?,
        trusted: row.get("trusted")?,
        trusted_chat: row.get("trusted_chat")?,
        project: row.get("project")?,
        workdir: row.get("workdir")?,
        workdir_on: row.get("workdir_on")?,
        live_turn: row.get::<_, Option<String>>("live_turn")?.map(TurnId),
        agent: row.get("agent")?,
        created_at: row.get::<_, Ts>("created_at")?.0,
        last_activity_at: row.get::<_, Ts>("last_activity_at")?.0,
    })
}

fn message_from_row(row: &Row<'_>) -> rusqlite::Result<MessageRow> {
    let seq: i64 = row.get("seq")?;
    let payload: Vec<u8> = row.get("payload")?;
    let payload: MessagePayload = rmp_serde::from_slice(&payload).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Blob, Box::new(e))
    })?;

    Ok(MessageRow {
        event_id: EventId(row.get("event_id")?),
        seq: u64::try_from(seq).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, seq))?,
        role: row.get::<_, Text<Role>>("role")?.0,
        sender: row
            .get::<_, Option<String>>("sender_person_id")?
            .map(PersonId),
        blocks: payload.blocks,
        external_id: row.get("external_id")?,
        reply_to_external_id: row.get("reply_to_external_id")?,
        turn: row.get::<_, Option<String>>("turn_id")?.map(TurnId),
        created_at: row.get::<_, Ts>("created_at")?.0,
    })
}
