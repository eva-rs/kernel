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
    /// `None` — якорь страницы исчез (purge между страницами) или не виден caller'у.
    pub fn list(
        &self,
        filter: &ChatFilter,
        after: Option<ChatId>,
        limit: u16,
    ) -> DbResult<Option<Vec<ChatRow>>> {
        let after_at = match after {
            Some(id) => match self.last_activity(id, &filter.surfaces)? {
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

    /// Якорь под тем же фильтром видимости, что и страница: чужой якорь —
    /// как исчезнувший, иначе он выдавал бы существование и время чужого чата.
    fn last_activity(&self, id: ChatId, surfaces: &SurfaceFilter) -> DbResult<Option<Timestamp>> {
        Ok(self
            .0
            .query_row(
                "SELECT c.last_activity_at FROM chats c
                   JOIN folders f ON f.id = c.folder_id
                   JOIN surfaces s ON s.id = f.surface_id
                  WHERE c.id = ?1 AND (?2 IS NULL OR s.module = ?2)",
                (id.0, surfaces.module()),
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

    /// Строки `title` всех чатов приводятся к `COALESCE(external_title, title)`:
    /// индексатор зовёт это каждой пачкой, хуков в rename не нужно. Сколько
    /// строк создано, заменено или снято.
    pub fn sync_titles(&self) -> DbResult<usize> {
        let removed = self.0.execute(
            "DELETE FROM chat_speech WHERE kind = 'title' AND chat_id IN
               (SELECT id FROM chats WHERE COALESCE(external_title, title) IS NULL)",
            [],
        )?;
        let upserted = self.0.execute(
            "INSERT INTO chat_speech (chat_id, seq, chunk_no, kind, text)
             SELECT c.id, NULL, 0, 'title', COALESCE(c.external_title, c.title) FROM chats c
              WHERE COALESCE(c.external_title, c.title) IS NOT NULL
                AND NOT EXISTS (SELECT 1 FROM chat_speech s
                                 WHERE s.chat_id = c.id AND s.kind = 'title'
                                   AND s.text = COALESCE(c.external_title, c.title))
             ON CONFLICT (chat_id) WHERE kind = 'title' DO UPDATE SET text = excluded.text",
            [],
        )?;
        Ok(removed + upserted)
    }

    pub fn move_to(&self, chat: ChatId, folder: FolderId) -> DbResult<()> {
        self.0.execute(
            "UPDATE chats SET folder_id = ?2 WHERE id = ?1",
            (chat.0, folder.0),
        )?;
        Ok(())
    }

    /// `(id, trusted_chat)` чатов папки, наследующих trust: только их задевает
    /// правка или удаление папки.
    pub fn inherited_trust(&self, folder: FolderId) -> DbResult<Vec<(ChatId, bool)>> {
        let mut stmt = self.0.prepare(
            "SELECT c.id, COALESCE(f.trusted, s.trusted, 0) FROM chats c
               JOIN folders f ON f.id = c.folder_id
               JOIN surfaces s ON s.id = f.surface_id
              WHERE c.folder_id = ?1 AND c.trusted IS NULL",
        )?;
        let rows = stmt
            .query_map([folder.0], |row| Ok((ChatId(row.get(0)?), row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Чаты снимка `before`, чей trust с тех пор сменился: граница пересечена,
    /// открытый сегмент каждого помечается к сбросу (`05 §2.3`).
    pub fn reset_crossed(&self, before: &[(ChatId, bool)]) -> DbResult<Vec<ChatId>> {
        let mut stmt = self.0.prepare(
            "SELECT COALESCE(c.trusted, f.trusted, s.trusted, 0) FROM chats c
               JOIN folders f ON f.id = c.folder_id
               JOIN surfaces s ON s.id = f.surface_id
              WHERE c.id = ?1",
        )?;
        let mut crossed = Vec::new();
        for &(chat, was) in before {
            let now: Option<bool> = stmt.query_row([chat.0], |row| row.get(0)).optional()?;
            if now.is_some_and(|now| now != was) {
                self.mark_reset(chat)?;
                crossed.push(chat);
            }
        }
        Ok(crossed)
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

    pub fn messages_before(
        &self,
        chat: ChatId,
        before: u64,
        count: u16,
    ) -> DbResult<Vec<MessageRow>> {
        let mut stmt = self.0.prepare(
            "SELECT m.event_id, m.seq, m.role, m.sender_person_id, m.external_id,
                    m.reply_to_external_id, m.turn_id, e.payload, e.created_at
               FROM messages m JOIN events e ON e.id = m.event_id
              WHERE m.chat_id = ?1 AND m.seq < ?2 ORDER BY m.seq DESC LIMIT ?3",
        )?;
        let before = i64::try_from(before).unwrap_or(i64::MAX);
        Ok(stmt
            .query_map((chat.0, before, count), message_from_row)?
            .collect::<Result<_, _>>()?)
    }

    pub fn set_external_title(&self, chat: ChatId, title: Option<&str>) -> DbResult<()> {
        self.0.execute(
            "UPDATE chats SET external_title = ?2 WHERE id = ?1",
            (chat.0, title),
        )?;
        Ok(())
    }

    /// Название запускает только первая человеческая реплика обычного чата.
    pub fn needs_title(&self, chat: ChatId, first: EventId) -> DbResult<bool> {
        Ok(self.0.query_row(
            "SELECT EXISTS (
               SELECT 1 FROM chats c JOIN folders f ON f.id = c.folder_id
                WHERE c.id = ?1 AND c.title IS NULL AND c.external_title IS NULL
                  AND c.deleted_at IS NULL
                  AND NOT (f.builtin = 1 AND (f.name = 'agents'
                       OR (f.surface_id = 'system' AND f.name IN ('cron', 'sleep', 'notify'))))
                  AND ?2 = (SELECT MIN(m.event_id) FROM messages m
                             WHERE m.chat_id = c.id AND m.role = 'user'
                               AND COALESCE(m.sender_person_id, '') <> 'p0'))",
            (chat.0, first.0),
            |row| row.get(0),
        )?)
    }

    /// Поздний результат генерации не перезаписывает ручное название.
    pub fn set_generated_title(&self, chat: ChatId, title: &str) -> DbResult<bool> {
        Ok(self.0.execute(
            "UPDATE chats SET title = ?2 WHERE id = ?1 AND title IS NULL AND deleted_at IS NULL",
            (chat.0, title),
        )? > 0)
    }

    pub fn promote(&self, chat: ChatId) -> DbResult<()> {
        self.0.execute(
            "UPDATE chats SET kind = 'persistent' WHERE id = ?1 AND kind = 'ephemeral'",
            [chat.0],
        )?;
        Ok(())
    }

    /// Реплики, ждущие индексации поиска (`05 §7.1`), по возрастанию `event_id`.
    pub fn unindexed(&self, limit: u32) -> DbResult<Vec<Unindexed>> {
        let mut stmt = self.0.prepare(
            "SELECT m.chat_id, m.seq, m.role, e.payload, COALESCE(c.trusted, f.trusted, s.trusted, 0) AS trusted_chat
               FROM messages m JOIN events e ON e.id = m.event_id
               JOIN chats c ON c.id = m.chat_id JOIN folders f ON f.id = c.folder_id JOIN surfaces s ON s.id = f.surface_id
              WHERE m.indexed_at IS NULL ORDER BY m.event_id LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit], |row| {
                let payload: Vec<u8> = row.get("payload")?;
                let payload: SpeechPayload = rmp_serde::from_slice(&payload).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Blob,
                        Box::new(e),
                    )
                })?;
                let role = row.get::<_, Text<Role>>("role")?.0;
                Ok(Unindexed {
                    chat: ChatId(row.get("chat_id")?),
                    seq: row.get("seq")?,
                    role,
                    trusted_chat: row.get("trusted_chat")?,
                    // Ввод без спикера — текст soul (`01 §1.2`); у assistant спикера нет всегда.
                    machine: role == Role::User && payload.speaker.is_none(),
                    blocks: payload.blocks,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Чанки реплики заново: повторная индексация не плодит дубли.
    pub fn clear_speech(&self, chat: ChatId, seq: i64) -> DbResult<()> {
        self.0.execute(
            "DELETE FROM chat_speech WHERE chat_id = ?1 AND seq = ?2",
            (chat.0, seq),
        )?;
        Ok(())
    }

    pub fn insert_speech(
        &self,
        chat: ChatId,
        seq: i64,
        chunk_no: u32,
        text: &str,
    ) -> DbResult<i64> {
        Ok(self.0.query_row(
            "INSERT INTO chat_speech (chat_id, seq, chunk_no, kind, text) VALUES (?1, ?2, ?3, 'speech', ?4) RETURNING id",
            (chat.0, seq, chunk_no, text),
            |row| row.get(0),
        )?)
    }

    pub fn mark_indexed(&self, chat: ChatId, seq: i64, now: Timestamp) -> DbResult<()> {
        self.0.execute(
            "UPDATE messages SET indexed_at = ?3 WHERE chat_id = ?1 AND seq = ?2",
            (chat.0, seq, Ts(now)),
        )?;
        Ok(())
    }

    /// Чанки без вектора активной модели; речь trusted-чатов — только локальному эмбеддеру.
    pub fn speech_embed_queue(
        &self,
        model: &ModelId,
        include_trusted: bool,
        limit: u32,
    ) -> DbResult<Vec<(i64, String)>> {
        let mut stmt = self.0.prepare(
            "SELECT s.id, s.text FROM chat_speech s
               JOIN chats c ON c.id = s.chat_id JOIN folders f ON f.id = c.folder_id JOIN surfaces sf ON sf.id = f.surface_id
               LEFT JOIN chat_speech_vec v ON v.chunk_id = s.id
              WHERE (v.chunk_id IS NULL OR v.embed_model <> :model)
                AND (:trusted OR COALESCE(c.trusted, f.trusted, sf.trusted, 0) = 0)
              ORDER BY s.id LIMIT :limit",
        )?;
        let rows = stmt
            .query_map(
                named_params! { ":model": model.as_str(), ":trusted": include_trusted, ":limit": limit },
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn set_speech_vector(&self, chunk: i64, model: &ModelId, vector: &[f32]) -> DbResult<()> {
        self.0.execute(
            "INSERT OR REPLACE INTO chat_speech_vec (chunk_id, embedding, embed_model) VALUES (?1, ?2, ?3)",
            (chunk, super::memory::encode(vector), model.as_str()),
        )?;
        Ok(())
    }

    /// Лексические кандидаты поиска: `(chunk_id, bm25)` по возрастанию.
    pub fn speech_fts(
        &self,
        query: &str,
        scope: &SearchScope,
        limit: u32,
    ) -> DbResult<Vec<(i64, f64)>> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT s.id, bm25(chat_speech_fts) AS rank FROM chat_speech_fts
               JOIN chat_speech s ON s.id = chat_speech_fts.rowid {}
              WHERE chat_speech_fts MATCH :query AND {}
              ORDER BY rank, s.id LIMIT :limit",
            SearchScope::JOINS,
            SearchScope::WHERE
        );
        let mut stmt = self.0.prepare(&sql)?;
        let params = scope.params();
        let mut params: Vec<(&str, &dyn rusqlite::ToSql)> = params
            .iter()
            .map(|(k, v)| (*k, v as &dyn rusqlite::ToSql))
            .collect();
        params.push((":query", &query));
        params.push((":limit", &limit));
        let rows = stmt
            .query_map(params.as_slice(), |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Векторы чанков в scope под активной моделью: cos считает вызывающий.
    pub fn speech_vectors(
        &self,
        scope: &SearchScope,
        model: &ModelId,
    ) -> DbResult<Vec<(i64, Vec<f32>)>> {
        let sql = format!(
            "SELECT s.id, v.embedding FROM chat_speech s
               JOIN chat_speech_vec v ON v.chunk_id = s.id {}
              WHERE v.embed_model = :model AND {}",
            SearchScope::JOINS,
            SearchScope::WHERE
        );
        let mut stmt = self.0.prepare(&sql)?;
        let params = scope.params();
        let mut params: Vec<(&str, &dyn rusqlite::ToSql)> = params
            .iter()
            .map(|(k, v)| (*k, v as &dyn rusqlite::ToSql))
            .collect();
        let model = model.as_str();
        params.push((":model", &model));
        let rows = stmt
            .query_map(params.as_slice(), |row| {
                let blob: Vec<u8> = row.get(1)?;
                Ok((row.get(0)?, super::memory::decode(&blob)))
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Хиты по id чанков, в порядке id.
    pub fn speech_hits(&self, chunks: &[i64]) -> DbResult<Vec<SpeechHit>> {
        let mut stmt = self.0.prepare(
            "SELECT s.id, s.chat_id, COALESCE(c.external_title, c.title) AS title, s.seq, s.text,
                    m.role, m.sender_person_id, COALESCE(e.created_at, c.last_activity_at) AS at
               FROM chat_speech s JOIN chats c ON c.id = s.chat_id
               LEFT JOIN messages m ON m.chat_id = s.chat_id AND m.seq = s.seq
               LEFT JOIN events e ON e.id = m.event_id
              WHERE s.id = ?1",
        )?;
        let mut hits = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            let hit = stmt
                .query_row([chunk], |row| {
                    let seq: Option<i64> = row.get("seq")?;
                    Ok(SpeechHit {
                        chunk: row.get("id")?,
                        chat: ChatId(row.get("chat_id")?),
                        title: row.get("title")?,
                        seq: seq.and_then(|s| u64::try_from(s).ok()),
                        role: row.get::<_, Option<Text<Role>>>("role")?.map(|r| r.0),
                        sender: row
                            .get::<_, Option<String>>("sender_person_id")?
                            .map(PersonId),
                        text: row.get("text")?,
                        at: row.get::<_, Ts>("at")?.0,
                    })
                })
                .optional()?;
            hits.extend(hit);
        }
        Ok(hits)
    }

    /// Последние `n` чанков речи чата, старые первыми: запрос темы hot-блока (`03 §5`).
    pub fn last_speech(&self, chat: ChatId, n: u32) -> DbResult<Vec<String>> {
        let mut stmt = self.0.prepare(
            "SELECT text FROM chat_speech WHERE chat_id = ?1 AND kind = 'speech'
              ORDER BY seq DESC, chunk_no DESC LIMIT ?2",
        )?;
        let mut rows: Vec<String> = stmt
            .query_map((chat.0, n), |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        rows.reverse();
        Ok(rows)
    }
}

/// Реплика без строк индекса: блоки и trust её чата.
#[data]
pub struct Unindexed {
    pub chat: ChatId,
    pub seq: i64,
    pub role: Role,
    pub trusted_chat: bool,
    /// `speaker = None` у `user` — текст soul, не речь (`01 §1.2`).
    pub machine: bool,
    pub blocks: Vec<Block>,
}

/// Часть payload `message`, нужная индексатору: блоки и был ли спикер.
#[data]
struct SpeechPayload {
    blocks: Vec<Block>,
    #[serde(default)]
    speaker: Option<serde_json::Value>,
}

/// Строка индекса для хита поиска; `seq = None` — название чата.
#[data]
pub struct SpeechHit {
    pub chunk: i64,
    pub chat: ChatId,
    pub title: Option<String>,
    pub seq: Option<u64>,
    pub role: Option<Role>,
    pub sender: Option<PersonId>,
    pub text: String,
    pub at: Timestamp,
}

/// Scope поиска (`05 §7.3`): фильтр в `WHERE`, не пост-фильтр.
#[data]
#[derive(Default)]
pub struct SearchScope {
    /// Тур вне `master ∧ trusted`: его чат плюс неудалённые untrusted; `None` — всё.
    pub narrow_to: Option<ChatId>,
    pub surface: Option<SurfaceId>,
    pub folder: Option<FolderId>,
    pub include_deleted: bool,
    pub since: Option<Timestamp>,
}

impl SearchScope {
    const JOINS: &'static str = "JOIN chats c ON c.id = s.chat_id
               JOIN folders f ON f.id = c.folder_id JOIN surfaces sf ON sf.id = f.surface_id
               LEFT JOIN messages m ON m.chat_id = s.chat_id AND m.seq = s.seq
               LEFT JOIN events e ON e.id = m.event_id";

    const WHERE: &'static str = "(:narrow IS NULL OR s.chat_id = :narrow
                    OR (COALESCE(c.trusted, f.trusted, sf.trusted, 0) = 0 AND c.deleted_at IS NULL))
                AND (:deleted OR c.deleted_at IS NULL OR s.chat_id = :narrow)
                AND (:surface IS NULL OR f.surface_id = :surface)
                AND (:folder IS NULL OR c.folder_id = :folder)
                AND (:since IS NULL OR COALESCE(e.created_at, c.last_activity_at) >= :since)";

    fn params(&self) -> [(&'static str, rusqlite::types::Value); 5] {
        use rusqlite::types::Value;
        [
            (
                ":narrow",
                self.narrow_to.map_or(Value::Null, |c| Value::Integer(c.0)),
            ),
            (
                ":deleted",
                Value::Integer(i64::from(self.include_deleted && self.narrow_to.is_none())),
            ),
            (
                ":surface",
                self.surface
                    .as_ref()
                    .map_or(Value::Null, |s| Value::Text(s.0.clone())),
            ),
            (
                ":folder",
                self.folder.map_or(Value::Null, |f| Value::Integer(f.0)),
            ),
            (
                ":since",
                self.since
                    .and_then(|t| Ts(t).nanos().ok())
                    .map_or(Value::Null, Value::Integer),
            ),
        ]
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
