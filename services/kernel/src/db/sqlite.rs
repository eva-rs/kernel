//! Соединения SQLite (`10 §7`): один писатель за async-мьютексом и пул
//! читателей; каждый запрос — в `spawn_blocking`.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, TransactionBehavior};

use crate::db::{Db, DbError, DbResult, Tx, math};

type Shared = Arc<Mutex<Connection>>;

/// SQLite-файл ядра либо `:memory:` (одно соединение на всех).
#[derive(Clone)]
pub struct Sqlite {
    writer: Arc<Writer>,
    pool: Arc<Pool>,
}

struct Writer {
    /// Очередь писателей без занятых блокирующих потоков.
    gate: tokio::sync::Mutex<()>,
    conn: Shared,
}

struct Pool {
    /// `None` — база в памяти: читатели делят соединение писателя.
    path: Option<PathBuf>,
    busy_timeout: Duration,
    keep: usize,
    idle: Mutex<Vec<Shared>>,
    memory: Option<Shared>,
}

pub const MEMORY: &str = ":memory:";

impl Sqlite {
    /// Открывает писателя; `readers` — сколько читающих соединений держать.
    pub async fn open(path: &Path, busy_timeout: Duration, readers: usize) -> DbResult<Self> {
        let memory = path.as_os_str() == MEMORY;
        let owned = path.to_path_buf();
        let conn = blocking(move || open_writer(&owned, memory, busy_timeout)).await??;
        let conn = Arc::new(Mutex::new(conn));

        Ok(Self {
            writer: Arc::new(Writer {
                gate: tokio::sync::Mutex::new(()),
                conn: conn.clone(),
            }),
            pool: Arc::new(Pool {
                path: (!memory).then(|| path.to_path_buf()),
                busy_timeout,
                keep: readers,
                idle: Mutex::new(Vec::new()),
                memory: memory.then_some(conn),
            }),
        })
    }
}

impl Db for Sqlite {
    async fn reader(&self) -> DbResult<Reader> {
        let pool = self.pool.clone();
        let idle = match &pool.memory {
            Some(conn) => Some(conn.clone()),
            None => lock(&pool.idle).pop(),
        };
        let conn = match idle {
            Some(conn) => conn,
            None => {
                let (path, busy) = (pool.path.clone(), pool.busy_timeout);
                let conn = blocking(move || match path {
                    Some(path) => open_reader(&path, busy),
                    None => Err(DbError::Sqlite("reader of an in-memory database".into())),
                })
                .await??;
                Arc::new(Mutex::new(conn))
            }
        };

        Ok(Reader {
            conn: Some(conn),
            pool,
        })
    }

    async fn write<T, E, F>(&self, f: F) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<DbError> + Send + 'static,
        F: FnOnce(&Tx<'_>) -> Result<T, E> + Send + 'static,
    {
        let _gate = self.writer.gate.lock().await;
        let conn = self.writer.conn.clone();
        blocking(move || {
            let mut conn = lock(&conn);
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(DbError::from)?;
            let value = f(&tx)?;
            tx.commit().map_err(DbError::from)?;
            Ok(value)
        })
        .await
        .map_err(E::from)?
    }

    async fn exclusive<T, E, F>(&self, f: F) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<DbError> + Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, E> + Send + 'static,
    {
        let _gate = self.writer.gate.lock().await;
        let conn = self.writer.conn.clone();
        blocking(move || f(&mut lock(&conn)))
            .await
            .map_err(E::from)?
    }
}

/// Читающее соединение сессии; на drop возвращается в пул.
pub struct Reader {
    conn: Option<Shared>,
    pool: Arc<Pool>,
}

impl Reader {
    /// Запрос на этом соединении в блокирующем потоке.
    pub async fn run<T, E, F>(&self, f: F) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<DbError> + Send + 'static,
        F: FnOnce(&Connection) -> Result<T, E> + Send + 'static,
    {
        let Some(conn) = self.conn.clone() else {
            return Err(DbError::Task("reader is already released".into()).into());
        };
        blocking(move || f(&lock(&conn))).await.map_err(E::from)?
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        let Some(conn) = self.conn.take() else {
            return;
        };
        if self.pool.memory.is_some() {
            return;
        }
        let mut idle = lock(&self.pool.idle);
        if idle.len() < self.pool.keep {
            idle.push(conn);
        }
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> DbResult<T> {
    Ok(tokio::task::spawn_blocking(f).await?)
}

/// Паника под замком не портит соединение: транзакция откатывается на drop.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
}

fn open_writer(path: &Path, memory: bool, busy: Duration) -> DbResult<Connection> {
    let conn = if memory {
        Connection::open_in_memory()?
    } else {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(|e| DbError::Sqlite(format!("create {}: {e}", dir.display())))?;
        }
        let conn = Connection::open_with_flags(path, flags())?;
        // Действует только до первой таблицы: на новом файле.
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))?;
        conn
    };
    prepare(&conn, busy)?;
    Ok(conn)
}

fn open_reader(path: &Path, busy: Duration) -> DbResult<Connection> {
    let conn = Connection::open_with_flags(path, flags())?;
    prepare(&conn, busy)?;
    conn.pragma_update(None, "query_only", true)?;
    Ok(conn)
}

/// PRAGMA каждого соединения (`10 §7`).
fn prepare(conn: &Connection, busy: Duration) -> DbResult<()> {
    conn.busy_timeout(busy)?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    math::install(conn)
}
