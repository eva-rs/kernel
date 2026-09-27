//! Таблица `blobs` (`07 §6`, `10 §2.7`).

use eva::{data, time::Timestamp};
use rusqlite::{Connection, OptionalExtension as _, Row, named_params};

use eva_protocol::types::{BlobId, ChatId, ModuleId, SurfaceId, blob::Kind};

use crate::db::{
    DbResult,
    chats::ChatRef,
    sql::{Text, Ts},
    surfaces::SurfaceRef,
};

#[data(copy)]
pub enum Source {
    Upload,
    Tool,
    Generated,
}

#[data]
pub struct BlobRow {
    pub id: BlobId,
    pub kind: Kind,
    /// Чат и его surface для `Policy`; `None` — чат удалён или его не было.
    pub chat: Option<ChatRef>,
    pub name: String,
    pub media_type: String,
    pub size: u64,
    /// blake3 содержимого, 32 байта.
    pub hash: Vec<u8>,
    pub source: Source,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
}

/// Минимальный вес blob'а в квоте: каталог, файл и строка есть и у пустого.
pub const BLOB_FLOOR: u64 = 4096;

pub struct Blobs<'c>(pub &'c Connection);

impl Blobs<'_> {
    pub fn get(&self, id: &BlobId) -> DbResult<Option<BlobRow>> {
        Ok(self
            .0
            .query_row(
                "SELECT b.id, b.kind, b.chat_id, b.name, b.media_type, b.size, b.hash, b.source,
                        b.created_at, b.expires_at, s.id AS surface, s.module
                 FROM blobs b
                 LEFT JOIN chats c ON c.id = b.chat_id
                 LEFT JOIN folders f ON f.id = c.folder_id
                 LEFT JOIN surfaces s ON s.id = f.surface_id
                 WHERE b.id = ?1",
                [id.as_str()],
                from_row,
            )
            .optional()?)
    }

    /// Вес живых blob'ов в чатах surface'ов владельца, каждый не меньше
    /// `BLOB_FLOOR`; `None` — surface без модуля.
    pub fn live_bytes(&self, owner: Option<&ModuleId>, now: Timestamp) -> DbResult<u64> {
        let bytes: i64 = self.0.query_row(
            "SELECT COALESCE(SUM(MAX(b.size, ?3)), 0)
             FROM blobs b
             JOIN chats c ON c.id = b.chat_id
             JOIN folders f ON f.id = c.folder_id
             JOIN surfaces s ON s.id = f.surface_id
             WHERE b.expires_at > ?2 AND s.module IS ?1",
            (owner.map(ModuleId::as_str), Ts(now), BLOB_FLOOR as i64),
            |row| row.get(0),
        )?;
        Ok(u64::try_from(bytes).unwrap_or(0))
    }

    pub fn insert(&self, row: &BlobRow) -> DbResult<()> {
        self.0.execute(
            "INSERT INTO blobs (id, kind, chat_id, name, media_type, size, hash, source, created_at, expires_at)
             VALUES (:id, :kind, :chat, :name, :media_type, :size, :hash, :source, :created_at, :expires_at)",
            named_params! {
                ":id": row.id.as_str(),
                ":kind": Text(row.kind),
                ":chat": row.chat.as_ref().map(|c| c.id.get()),
                ":name": row.name,
                ":media_type": row.media_type,
                ":size": i64::try_from(row.size)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
                ":hash": row.hash,
                ":source": Text(row.source),
                ":created_at": Ts(row.created_at),
                ":expires_at": Ts(row.expires_at),
            },
        )?;
        Ok(())
    }
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<BlobRow> {
    let chat = match (
        row.get::<_, Option<i64>>("chat_id")?,
        row.get::<_, Option<String>>("surface")?,
    ) {
        (Some(chat), Some(surface)) => Some(ChatRef {
            id: ChatId(chat),
            surface: SurfaceRef {
                id: SurfaceId(surface),
                module: row.get::<_, Option<String>>("module")?.map(ModuleId),
            },
        }),
        _ => None,
    };
    let size: i64 = row.get("size")?;

    Ok(BlobRow {
        id: BlobId(row.get("id")?),
        kind: row.get::<_, Text<Kind>>("kind")?.0,
        chat,
        name: row.get("name")?,
        media_type: row.get("media_type")?,
        size: u64::try_from(size).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Integer,
                Box::new(e),
            )
        })?,
        hash: row.get("hash")?,
        source: row.get::<_, Text<Source>>("source")?.0,
        created_at: row.get::<_, Ts>("created_at")?.0,
        expires_at: row.get::<_, Ts>("expires_at")?.0,
    })
}
