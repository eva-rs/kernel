//! Таблица `surfaces` (`05 §1.1`, `10 §2.5`).

use eva::{data, time::Timestamp};
use rusqlite::{Connection, OptionalExtension as _, Row, named_params};

use eva_protocol::types::{Effort, ModelId, ModuleId, SurfaceId, Thinking, surface::Builtin};

use crate::db::{
    DbResult,
    sql::{Level, Text, Ts},
};

pub const LOCAL: &str = "local";
pub const SYSTEM: &str = "system";

/// Id встроенного surface.
pub const fn builtin_id(builtin: Builtin) -> &'static str {
    match builtin {
        Builtin::Local => LOCAL,
        Builtin::System => SYSTEM,
    }
}

#[data]
pub struct SurfaceRow {
    pub id: SurfaceId,
    pub name: String,
    pub module: Option<ModuleId>,
    pub rules: Option<String>,
    pub trusted: Option<bool>,
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
    pub thinking: Option<Thinking>,
    pub compact_at: Option<f32>,
    pub builtin: Option<Builtin>,
    pub created_at: Timestamp,
}

impl SurfaceRow {
    pub fn surface_ref(&self) -> SurfaceRef {
        SurfaceRef {
            id: self.id.clone(),
            module: self.module.clone(),
        }
    }
}

/// Чей объект: `Policy` решает по нему, не по строке целиком.
#[data]
#[derive(Eq, Hash)]
pub struct SurfaceRef {
    pub id: SurfaceId,
    /// Создатель; `None` у builtin `local`/`system`.
    pub module: Option<ModuleId>,
}

/// Какие surface видит caller: фильтр идёт в `WHERE`, не пост-фильтром.
#[data]
pub enum SurfaceFilter {
    All,
    Module(ModuleId),
}

impl SurfaceFilter {
    /// `module` для `WHERE (:module IS NULL OR module = :module)`.
    pub fn module(&self) -> Option<&str> {
        match self {
            Self::All => None,
            Self::Module(module) => Some(module.as_str()),
        }
    }
}

const COLUMNS: &str =
    "id, name, module, rules, trusted, model, effort, thinking, compact_at, builtin, created_at";

pub struct Surfaces<'c>(pub &'c Connection);

impl Surfaces<'_> {
    pub fn get(&self, id: &SurfaceId) -> DbResult<Option<SurfaceRow>> {
        let sql = format!("SELECT {COLUMNS} FROM surfaces WHERE id = ?1");
        Ok(self.0.query_row(&sql, [id.as_str()], from_row).optional()?)
    }

    /// Keyset по `id DESC`: `after` — последний id прошлой страницы.
    pub fn list(
        &self,
        filter: &SurfaceFilter,
        after: Option<&SurfaceId>,
        limit: u16,
    ) -> DbResult<Vec<SurfaceRow>> {
        let sql = format!(
            "SELECT {COLUMNS} FROM surfaces
             WHERE (:module IS NULL OR module = :module) AND (:after IS NULL OR id < :after)
             ORDER BY id DESC LIMIT :limit"
        );
        let mut stmt = self.0.prepare(&sql)?;
        let rows = stmt
            .query_map(
                named_params! {
                    ":module": filter.module(),
                    ":after": after.map(SurfaceId::as_str),
                    ":limit": limit,
                },
                from_row,
            )?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Новая строка целиком: surface модуля или засеянный builtin.
    pub fn insert(&self, row: &SurfaceRow) -> DbResult<()> {
        self.0.execute(
            &format!(
                "INSERT INTO surfaces ({COLUMNS})
                 VALUES (:id, :name, :module, :rules, :trusted, :model, :effort, :thinking, :compact_at, :builtin, :created_at)"
            ),
            named_params! {
                ":id": row.id.as_str(),
                ":name": row.name,
                ":module": row.module.as_ref().map(ModuleId::as_str),
                ":rules": row.rules,
                ":trusted": row.trusted,
                ":model": row.model.as_ref().map(ModelId::as_str),
                ":effort": row.effort.map(Level),
                ":thinking": row.thinking.map(Text),
                ":compact_at": row.compact_at,
                ":builtin": row.builtin.map(Text),
                ":created_at": Ts(row.created_at),
            },
        )?;
        Ok(())
    }

    /// Засев builtin: повторный старт строку не трогает.
    pub fn insert_builtin(&self, builtin: Builtin, name: &str, now: Timestamp) -> DbResult<()> {
        self.0.execute(
            "INSERT OR IGNORE INTO surfaces (id, name, module, trusted, builtin, created_at)
             VALUES (?1, ?2, NULL, 1, ?3, ?4)",
            (builtin_id(builtin), name, Text(builtin), Ts(now)),
        )?;
        Ok(())
    }

    /// Изменяемые колонки строки; `id`, `module`, `builtin`, `created_at` неизменны.
    pub fn save(&self, row: &SurfaceRow) -> DbResult<()> {
        self.0.execute(
            "UPDATE surfaces
             SET name = :name, rules = :rules, trusted = :trusted, model = :model,
                 effort = :effort, thinking = :thinking, compact_at = :compact_at
             WHERE id = :id",
            named_params! {
                ":id": row.id.as_str(),
                ":name": row.name,
                ":rules": row.rules,
                ":trusted": row.trusted,
                ":model": row.model.as_ref().map(ModelId::as_str),
                ":effort": row.effort.map(Level),
                ":thinking": row.thinking.map(Text),
                ":compact_at": row.compact_at,
            },
        )?;
        Ok(())
    }
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<SurfaceRow> {
    Ok(SurfaceRow {
        id: SurfaceId(row.get("id")?),
        name: row.get("name")?,
        module: row.get::<_, Option<String>>("module")?.map(ModuleId),
        rules: row.get("rules")?,
        trusted: row.get("trusted")?,
        model: row.get::<_, Option<String>>("model")?.map(ModelId),
        effort: row.get::<_, Option<Level>>("effort")?.map(|l| l.0),
        thinking: row
            .get::<_, Option<Text<Thinking>>>("thinking")?
            .map(|t| t.0),
        compact_at: row.get("compact_at")?,
        builtin: row.get::<_, Option<Text<Builtin>>>("builtin")?.map(|t| t.0),
        created_at: row.get::<_, Ts>("created_at")?.0,
    })
}
