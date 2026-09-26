//! Таблица `folders` (`05 §1.2`, `10 §2.5`).

use eva::data;
use rusqlite::{Connection, OptionalExtension as _, Row, named_params};

use eva_protocol::types::{Effort, FolderId, ModelId, ModuleId, SurfaceId, Thinking};

use crate::db::{
    DbResult,
    sql::{Level, Text},
    surfaces::SurfaceRef,
};

/// Папка по умолчанию каждого surface.
pub const UNSORTED: &str = "Unsorted";
/// Папка чатов подагентов каждого surface.
pub const AGENTS: &str = "agents";
/// Служебные папки surface `system` (`00 §2`).
pub const SYSTEM_ONLY: [&str; 3] = ["cron", "sleep", "notify"];

/// Встроенные папки surface: `system` получает ещё служебные.
pub fn builtin_names(system: bool) -> impl Iterator<Item = &'static str> {
    let extra: &[&str] = if system { &SYSTEM_ONLY } else { &[] };
    [UNSORTED, AGENTS].into_iter().chain(extra.iter().copied())
}

/// Папка для `chats().create`: `Service` кладёт чат только в папку с trust `Unsorted`.
#[data]
pub struct FolderRef {
    pub id: FolderId,
    pub surface: SurfaceRef,
    /// `folder.trusted ?? surface.trusted ?? false`.
    pub trusted: bool,
    /// То же для `Unsorted` этого surface.
    pub unsorted_trusted: bool,
}

#[data]
pub struct FolderRow {
    pub id: FolderId,
    pub surface: SurfaceRef,
    pub name: String,
    pub builtin: bool,
    pub trusted: Option<bool>,
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
    pub thinking: Option<Thinking>,
    pub compact_at: Option<f32>,
    /// `trusted` surface папки: следующее звено цепочки `05 §2.2`.
    pub surface_trusted: Option<bool>,
}

impl FolderRow {
    /// `folder.trusted ?? surface.trusted ?? false`.
    pub fn effective_trust(&self) -> bool {
        self.trusted.or(self.surface_trusted).unwrap_or(false)
    }

    pub fn folder_ref(&self, unsorted: &Self) -> FolderRef {
        FolderRef {
            id: self.id,
            surface: self.surface.clone(),
            trusted: self.effective_trust(),
            unsorted_trusted: unsorted.effective_trust(),
        }
    }
}

const SELECT: &str = "SELECT f.id, f.surface_id, s.module AS surface_module, f.name, f.builtin,
       f.trusted, f.model, f.effort, f.thinking, f.compact_at, s.trusted AS surface_trusted
  FROM folders f JOIN surfaces s ON s.id = f.surface_id";

pub struct Folders<'c>(pub &'c Connection);

impl Folders<'_> {
    /// Встроенные папки surface; повторный вызов ничего не меняет.
    pub fn insert_builtin(&self, surface: &SurfaceId, system: bool) -> DbResult<()> {
        let mut stmt = self.0.prepare(
            "INSERT OR IGNORE INTO folders (surface_id, name, builtin) VALUES (?1, ?2, 1)",
        )?;
        for name in builtin_names(system) {
            stmt.execute((surface.as_str(), name))?;
        }
        Ok(())
    }

    pub fn get(&self, id: FolderId) -> DbResult<Option<FolderRow>> {
        let sql = format!("{SELECT} WHERE f.id = ?1");
        Ok(self.0.query_row(&sql, [id.0], from_row).optional()?)
    }

    pub fn by_name(&self, surface: &SurfaceId, name: &str) -> DbResult<Option<FolderRow>> {
        let sql = format!("{SELECT} WHERE f.surface_id = ?1 AND f.name = ?2");
        Ok(self
            .0
            .query_row(&sql, (surface.as_str(), name), from_row)
            .optional()?)
    }

    /// Keyset по `id DESC`; `surface` — фильтр в `WHERE`.
    pub fn list(
        &self,
        surface: Option<&SurfaceId>,
        after: Option<FolderId>,
        limit: u16,
    ) -> DbResult<Vec<FolderRow>> {
        let sql = format!(
            "{SELECT}
             WHERE (:surface IS NULL OR f.surface_id = :surface) AND (:after IS NULL OR f.id < :after)
             ORDER BY f.id DESC LIMIT :limit"
        );
        let mut stmt = self.0.prepare(&sql)?;
        let rows = stmt
            .query_map(
                named_params! {
                    ":surface": surface.map(SurfaceId::as_str),
                    ":after": after.map(|a| a.0),
                    ":limit": limit,
                },
                from_row,
            )?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn insert(&self, surface: &SurfaceId, name: &str) -> DbResult<FolderId> {
        let id = self.0.query_row(
            "INSERT INTO folders (surface_id, name, builtin) VALUES (?1, ?2, 0) RETURNING id",
            (surface.as_str(), name),
            |row| row.get(0),
        )?;
        Ok(FolderId(id))
    }

    /// Изменяемые колонки; `surface_id` и `builtin` неизменны.
    pub fn save(&self, row: &FolderRow) -> DbResult<()> {
        self.0.execute(
            "UPDATE folders
             SET name = :name, trusted = :trusted, model = :model, effort = :effort,
                 thinking = :thinking, compact_at = :compact_at
             WHERE id = :id",
            named_params! {
                ":id": row.id.0,
                ":name": row.name,
                ":trusted": row.trusted,
                ":model": row.model.as_ref().map(ModelId::as_str),
                ":effort": row.effort.map(Level),
                ":thinking": row.thinking.map(Text),
                ":compact_at": row.compact_at,
            },
        )?;
        Ok(())
    }

    /// Все чаты папки, удалённые тоже, — в `into`; число перенесённых.
    pub fn move_chats(&self, from: FolderId, into: FolderId) -> DbResult<u64> {
        let moved = self.0.execute(
            "UPDATE chats SET folder_id = ?2 WHERE folder_id = ?1",
            (from.0, into.0),
        )?;
        Ok(moved as u64)
    }

    pub fn delete(&self, id: FolderId) -> DbResult<()> {
        self.0
            .execute("DELETE FROM folders WHERE id = ?1", [id.0])?;
        Ok(())
    }
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<FolderRow> {
    Ok(FolderRow {
        id: FolderId(row.get("id")?),
        surface: SurfaceRef {
            id: SurfaceId(row.get("surface_id")?),
            module: row
                .get::<_, Option<String>>("surface_module")?
                .map(ModuleId),
        },
        name: row.get("name")?,
        builtin: row.get("builtin")?,
        trusted: row.get("trusted")?,
        model: row.get::<_, Option<String>>("model")?.map(ModelId),
        effort: row.get::<_, Option<Level>>("effort")?.map(|l| l.0),
        thinking: row
            .get::<_, Option<Text<Thinking>>>("thinking")?
            .map(|t| t.0),
        compact_at: row.get("compact_at")?,
        surface_trusted: row.get("surface_trusted")?,
    })
}
