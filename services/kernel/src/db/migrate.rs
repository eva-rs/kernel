//! Миграции (`10 §5`): встроенные нумерованные файлы, forward-only.
//! Применённые обязаны быть префиксом встроенных с теми же checksum, иначе
//! старт отказывает — бинарь старее базы или файл правили задним числом.

use eva::{data, time::Timestamp};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};

use crate::db::{DbError, sql::Ts};

pub struct Migration {
    pub version: u32,
    pub name: &'static str,
    pub sql: &'static str,
}

impl Migration {
    /// blake3 файла миграции.
    pub fn checksum(&self) -> [u8; 32] {
        *blake3::hash(self.sql.as_bytes()).as_bytes()
    }
}

/// Все миграции бинаря по возрастанию версии, без пропусков.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "init",
        sql: include_str!("migrations/0001_init.sql"),
    },
    Migration {
        version: 2,
        name: "module_kv_usage",
        sql: include_str!("migrations/0002_module_kv_usage.sql"),
    },
];

#[data(error)]
pub enum MigrateError {
    #[display("database has tables but no schema_migrations: not an eva database")]
    Foreign,
    #[display("database has migration {version} ({name}) unknown to this build")]
    Unknown { version: u32, name: String },
    #[display("migration {version} ({name}) differs from the applied one")]
    Checksum { version: u32, name: String },
    #[display("migration {version} ({name}) failed: {error}")]
    Failed {
        version: u32,
        name: String,
        error: String,
    },
    #[display("{_0}")]
    Db(#[from] DbError),
}

impl From<rusqlite::Error> for MigrateError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e.into())
    }
}

/// Применяет недостающие миграции; возвращает, сколько применено.
pub fn run(
    conn: &mut Connection,
    migrations: &[Migration],
    now: Timestamp,
) -> Result<usize, MigrateError> {
    let applied = applied(conn)?;

    for (i, (version, name, checksum)) in applied.iter().enumerate() {
        let known = migrations
            .get(i)
            .filter(|m| m.version == *version && m.name == name);
        let Some(known) = known else {
            return Err(MigrateError::Unknown {
                version: *version,
                name: name.clone(),
            });
        };
        if known.checksum().as_slice() != checksum.as_slice() {
            return Err(MigrateError::Checksum {
                version: *version,
                name: name.clone(),
            });
        }
    }

    let pending = migrations.get(applied.len()..).unwrap_or_default();
    for m in pending {
        let failed = |e: rusqlite::Error| MigrateError::Failed {
            version: m.version,
            name: m.name.to_owned(),
            error: e.to_string(),
        };

        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // `0001` создаёт саму `schema_migrations`, поэтому строка — после SQL.
        tx.execute_batch(m.sql).map_err(failed)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, name, checksum, applied_at) VALUES (?1, ?2, ?3, ?4)",
            params![m.version, m.name, m.checksum().as_slice(), Ts(now)],
        )?;
        tx.commit()?;
        conn.pragma_update(None, "user_version", m.version)?;

        log::info!(version = m.version, name = m.name; "migration applied");
    }

    Ok(pending.len())
}

type Applied = Vec<(u32, String, Vec<u8>)>;

fn applied(conn: &Connection) -> Result<Applied, MigrateError> {
    let tracked = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();

    if !tracked {
        let tables: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        return if tables == 0 {
            Ok(Vec::new())
        } else {
            Err(MigrateError::Foreign)
        };
    }

    let mut stmt =
        conn.prepare("SELECT version, name, checksum FROM schema_migrations ORDER BY version")?;
    let rows = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}
