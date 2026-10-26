//! Таблица `hands_grants` (`07 §3.1`, `10 §2.7`).

use eva::{data, time::Timestamp};
use rusqlite::{Connection, Row, named_params};

use eva_protocol::types::{ChatId, HandsGrantId, PersonId, SurfaceId, hands::GrantKind};

use crate::db::{
    DbResult,
    sql::{Text, Ts},
};

#[data]
pub struct GrantRow {
    pub id: HandsGrantId,
    pub endpoint: String,
    pub surface: SurfaceId,
    pub chat: Option<ChatId>,
    pub kind: GrantKind,
    pub prefix: String,
    pub issued_by: PersonId,
    pub created_at: Timestamp,
}

/// Фильтры `hands_grants.list`: каждый заданный сужает выборку.
#[data]
#[derive(Default)]
pub struct GrantFilter {
    pub endpoint: Option<String>,
    pub surface: Option<SurfaceId>,
    pub chat: Option<ChatId>,
}

/// Новый грант `allow_always` (`07 §3.1`).
#[data]
pub struct NewGrant {
    pub endpoint: String,
    pub surface: SurfaceId,
    pub chat: Option<ChatId>,
    pub kind: GrantKind,
    pub prefix: String,
    pub issued_by: PersonId,
    pub created_at: Timestamp,
}

const COLUMNS: &str = "id, endpoint, surface, chat_id, kind, prefix, issued_by, created_at";

pub struct HandsGrants<'c>(pub &'c Connection);

impl HandsGrants<'_> {
    /// Новые сверху.
    pub fn list(&self, filter: &GrantFilter) -> DbResult<Vec<GrantRow>> {
        let sql = format!(
            "SELECT {COLUMNS} FROM hands_grants
             WHERE (:endpoint IS NULL OR endpoint = :endpoint)
               AND (:surface IS NULL OR surface = :surface)
               AND (:chat IS NULL OR chat_id = :chat)
             ORDER BY id DESC"
        );
        let mut stmt = self.0.prepare(&sql)?;
        let rows = stmt
            .query_map(
                named_params! {
                    ":endpoint": filter.endpoint,
                    ":surface": filter.surface.as_ref().map(SurfaceId::as_str),
                    ":chat": filter.chat.map(ChatId::get),
                },
                from_row,
            )?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn insert(&self, grant: &NewGrant) -> DbResult<HandsGrantId> {
        let id = self.0.query_row(
            "INSERT INTO hands_grants (endpoint, surface, chat_id, kind, prefix, issued_by, created_at)
             VALUES (:endpoint, :surface, :chat, :kind, :prefix, :issued_by, :created_at)
             RETURNING id",
            named_params! {
                ":endpoint": grant.endpoint,
                ":surface": grant.surface.as_str(),
                ":chat": grant.chat.map(ChatId::get),
                ":kind": Text(grant.kind),
                ":prefix": grant.prefix,
                ":issued_by": grant.issued_by.as_str(),
                ":created_at": Ts(grant.created_at),
            },
            |row| row.get(0),
        )?;
        Ok(HandsGrantId(id))
    }

    /// Удалён ли грант: `false` — такого нет.
    pub fn delete(&self, id: HandsGrantId) -> DbResult<bool> {
        Ok(self
            .0
            .execute("DELETE FROM hands_grants WHERE id = ?1", [id.get()])?
            > 0)
    }
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<GrantRow> {
    Ok(GrantRow {
        id: HandsGrantId(row.get("id")?),
        endpoint: row.get("endpoint")?,
        surface: SurfaceId(row.get("surface")?),
        chat: row.get::<_, Option<i64>>("chat_id")?.map(ChatId),
        kind: row.get::<_, Text<GrantKind>>("kind")?.0,
        prefix: row.get("prefix")?,
        issued_by: PersonId(row.get("issued_by")?),
        created_at: row.get::<_, Ts>("created_at")?.0,
    })
}
