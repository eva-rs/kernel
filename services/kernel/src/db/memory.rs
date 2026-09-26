//! Таблицы `memory`, `memory_fts`, `memory_archive`, `memory_link`, `embed_cache` (`03`, `10 §2.3`).

use eva::{data, time::Timestamp};
use rusqlite::{Connection, OptionalExtension as _, Row, named_params};

use eva_protocol::types::{
    ChatId, MemoryId, ModelId, PersonId, RunId, SurfaceId,
    memory::{ArchiveReason, Kind, Visibility},
};

use crate::db::{
    DbError, DbResult,
    sql::{Text, Ts},
};

#[data]
pub struct MemoryRow {
    pub id: MemoryId,
    pub kind: Kind,
    pub text: String,
    pub name: Option<String>,
    pub description: Option<String>,
    /// Slug'и через пробел, как в колонке.
    pub tags: Option<String>,
    pub visibility: Visibility,
    pub importance: f64,
    pub confidence: f64,
    pub source_actor: Option<PersonId>,
    pub source_chat: Option<ChatId>,
    pub source_surface: Option<SurfaceId>,
    pub source_trusted_chat: Option<bool>,
    pub source_run: Option<RunId>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub consolidated_at: Option<Timestamp>,
    pub occurred_at: Option<Timestamp>,
    pub valid_from: Timestamp,
    pub valid_to: Option<Timestamp>,
    pub supersedes: Option<MemoryId>,
    pub version: u32,
    pub last_reinforced_at: Timestamp,
    pub access_count: f64,
    pub embed_model: Option<ModelId>,
    pub archive: Option<ArchiveRow>,
}

#[data]
pub struct ArchiveRow {
    pub reason: ArchiveReason,
    pub why: String,
    pub run: Option<RunId>,
    pub actor: Option<PersonId>,
    pub archived_at: Timestamp,
    pub restored_at: Option<Timestamp>,
}

impl ArchiveRow {
    /// Забыта: архив есть и restore его не снял.
    pub const fn is_open(&self) -> bool {
        self.restored_at.is_none()
    }
}

/// Фильтры `memory.list`; `None` — без условия.
#[data]
#[derive(Default)]
pub struct ListFilter {
    pub kind: Option<Kind>,
    pub visibility: Option<Visibility>,
    pub since: Option<Timestamp>,
}

const SELECT: &str = "SELECT m.id, m.kind, m.text, m.name, m.description, m.tags, m.visibility,
       m.importance, m.confidence, m.source_actor, m.source_chat, m.source_surface,
       m.source_trusted_chat, m.source_run_id, m.created_at, m.updated_at, m.consolidated_at,
       m.occurred_at, m.valid_from, m.valid_to, m.supersedes, m.version, m.last_reinforced_at,
       m.access_count, m.embed_model,
       a.reason AS a_reason, a.why AS a_why, a.run_id AS a_run_id, a.actor AS a_actor,
       a.archived_at AS a_archived_at, a.restored_at AS a_restored_at
  FROM memory m LEFT JOIN memory_archive a ON a.memory_id = m.id";

/// Вся цепочка версий `:id`: предки по `supersedes` и потомки.
const CHAIN: &str = "WITH RECURSIVE
  up(id, sup) AS (
    SELECT id, supersedes FROM memory WHERE id = :id
    UNION SELECT p.id, p.supersedes FROM memory p JOIN up ON p.id = up.sup),
  down(id) AS (
    SELECT id FROM memory WHERE id = :id
    UNION SELECT n.id FROM memory n JOIN down ON n.supersedes = down.id),
  chain(id) AS (SELECT id FROM up UNION SELECT id FROM down)";

pub struct Memory<'c>(pub &'c Connection);

impl Memory<'_> {
    pub fn get(&self, id: MemoryId) -> DbResult<Option<MemoryRow>> {
        let sql = format!("{SELECT} WHERE m.id = ?1");
        Ok(self.0.query_row(&sql, [id.get()], from_row).optional()?)
    }

    /// Текущие версии, keyset по `id DESC`; `since` — по `created_at`.
    pub fn list(
        &self,
        filter: &ListFilter,
        after: Option<MemoryId>,
        limit: u16,
    ) -> DbResult<Vec<MemoryRow>> {
        let sql = format!(
            "{SELECT}
              WHERE m.valid_to IS NULL
                AND (:kind IS NULL OR m.kind = :kind)
                AND (:visibility IS NULL OR m.visibility = :visibility)
                AND (:since IS NULL OR m.created_at >= :since)
                AND (:after IS NULL OR m.id < :after)
              ORDER BY m.id DESC LIMIT :limit"
        );
        let mut stmt = self.0.prepare(&sql)?;
        let rows = stmt
            .query_map(
                named_params! {
                    ":kind": filter.kind.map(Text),
                    ":visibility": filter.visibility.map(Text),
                    ":since": filter.since.map(Ts),
                    ":after": after.map(MemoryId::get),
                    ":limit": limit,
                },
                from_row,
            )?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Цепочка версий записи от корня к новейшей.
    pub fn chain(&self, id: MemoryId) -> DbResult<Vec<MemoryRow>> {
        let sql = format!(
            "{CHAIN} {SELECT} WHERE m.id IN (SELECT id FROM chain) ORDER BY m.version, m.valid_from, m.id"
        );
        let mut stmt = self.0.prepare(&sql)?;
        let rows = stmt
            .query_map(named_params! { ":id": id.get() }, from_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Текущая procedure с этим именем, кроме `except`: уникальный индекс `memory_procedure_name`.
    pub fn procedure_named(&self, name: &str, except: MemoryId) -> DbResult<Option<MemoryId>> {
        Ok(self
            .0
            .query_row(
                "SELECT id FROM memory
                  WHERE kind = 'procedure' AND valid_to IS NULL AND name = ?1 AND id <> ?2",
                (name, except.get()),
                |row| row.get(0).map(MemoryId),
            )
            .optional()?)
    }

    /// `restore` (`03 §9`): версия снова текущая, архив помечен; `access_count` не трогается.
    pub fn restore(&self, id: MemoryId, now: Timestamp) -> DbResult<()> {
        let reopened = self.0.execute(
            "UPDATE memory SET valid_to = NULL WHERE id = ?1",
            [id.get()],
        )?;
        let unarchived = self.0.execute(
            "UPDATE memory_archive SET restored_at = ?2 WHERE memory_id = ?1 AND restored_at IS NULL",
            (id.get(), Ts(now)),
        )?;
        if reopened == 1 && unarchived == 1 {
            Ok(())
        } else {
            Err(DbError::corrupt(format_args!(
                "restore of {id} touched {reopened} memory and {unarchived} archive rows"
            )))
        }
    }
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<MemoryRow> {
    let archive = match row.get::<_, Option<Text<ArchiveReason>>>("a_reason")? {
        Some(Text(reason)) => Some(ArchiveRow {
            reason,
            why: row.get("a_why")?,
            run: row.get::<_, Option<String>>("a_run_id")?.map(RunId),
            actor: row.get::<_, Option<String>>("a_actor")?.map(PersonId),
            archived_at: row.get::<_, Ts>("a_archived_at")?.0,
            restored_at: row.get::<_, Option<Ts>>("a_restored_at")?.map(|t| t.0),
        }),
        None => None,
    };

    Ok(MemoryRow {
        id: MemoryId(row.get("id")?),
        kind: row.get::<_, Text<Kind>>("kind")?.0,
        text: row.get("text")?,
        name: row.get("name")?,
        description: row.get("description")?,
        tags: row.get("tags")?,
        visibility: row.get::<_, Text<Visibility>>("visibility")?.0,
        importance: row.get("importance")?,
        confidence: row.get("confidence")?,
        source_actor: row.get::<_, Option<String>>("source_actor")?.map(PersonId),
        source_chat: row.get::<_, Option<i64>>("source_chat")?.map(ChatId),
        source_surface: row
            .get::<_, Option<String>>("source_surface")?
            .map(SurfaceId),
        source_trusted_chat: row.get("source_trusted_chat")?,
        source_run: row.get::<_, Option<String>>("source_run_id")?.map(RunId),
        created_at: row.get::<_, Ts>("created_at")?.0,
        updated_at: row.get::<_, Ts>("updated_at")?.0,
        consolidated_at: row.get::<_, Option<Ts>>("consolidated_at")?.map(|t| t.0),
        occurred_at: row.get::<_, Option<Ts>>("occurred_at")?.map(|t| t.0),
        valid_from: row.get::<_, Ts>("valid_from")?.0,
        valid_to: row.get::<_, Option<Ts>>("valid_to")?.map(|t| t.0),
        supersedes: row.get::<_, Option<i64>>("supersedes")?.map(MemoryId),
        version: row.get("version")?,
        last_reinforced_at: row.get::<_, Ts>("last_reinforced_at")?.0,
        access_count: row.get("access_count")?,
        embed_model: row.get::<_, Option<String>>("embed_model")?.map(ModelId),
        archive,
    })
}
