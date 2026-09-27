//! Соединения SQLite (`10 §7`): один писатель за async-мьютексом и не более
//! `db.readers` читателей; каждый запрос — в `spawn_blocking`.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use tokio::sync::Semaphore;

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
    /// Разрешение — право на одно читающее соединение на время запроса.
    permits: Arc<Semaphore>,
    readers: usize,
    idle: Mutex<Vec<Connection>>,
    memory: Option<Shared>,
}

pub const MEMORY: &str = ":memory:";

impl Sqlite {
    /// Открывает писателя; `readers` — потолок читающих соединений.
    pub async fn open(path: &Path, busy_timeout: Duration, readers: usize) -> DbResult<Self> {
        let memory = path.as_os_str() == MEMORY;
        let owned = path.to_path_buf();
        let conn = blocking(move || open_writer(&owned, memory, busy_timeout)).await??;
        let conn = Arc::new(Mutex::new(conn));
        let readers = readers.max(1);

        Ok(Self {
            writer: Arc::new(Writer {
                gate: tokio::sync::Mutex::new(()),
                conn: conn.clone(),
            }),
            pool: Arc::new(Pool {
                path: (!memory).then(|| path.to_path_buf()),
                busy_timeout,
                permits: Arc::new(Semaphore::new(readers)),
                readers,
                idle: Mutex::new(Vec::new()),
                memory: memory.then_some(conn),
            }),
        })
    }
}

impl Db for Sqlite {
    async fn reader(&self) -> DbResult<Reader> {
        Ok(Reader {
            pool: self.pool.clone(),
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

/// Доступ сессии к пулу читателей: соединение берётся на время одного
/// запроса, поэтому держать `Reader` долго — не значит держать соединение.
pub struct Reader {
    pool: Arc<Pool>,
}

impl Reader {
    /// Запрос на свободном читающем соединении в блокирующем потоке.
    pub async fn run<T, E, F>(&self, f: F) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<DbError> + Send + 'static,
        F: FnOnce(&Connection) -> Result<T, E> + Send + 'static,
    {
        let pool = self.pool.clone();
        if let Some(conn) = pool.memory.clone() {
            return blocking(move || f(&lock(&conn))).await.map_err(E::from)?;
        }

        let permit = tokio::time::timeout(pool.busy_timeout, pool.permits.clone().acquire_owned())
            .await
            .map_err(|_| {
                DbError::Sqlite(format!(
                    "all {} readers stayed busy for {:?}",
                    pool.readers, pool.busy_timeout
                ))
            })?
            .map_err(|e| DbError::Task(format!("reader pool: {e}")))?;
        // Разрешение уходит в поток: отменённый запрос держит его, пока соединение занято.
        blocking(move || {
            let _permit = permit;
            let conn = match lock(&pool.idle).pop() {
                Some(conn) => conn,
                None => pool.open().map_err(E::from)?,
            };
            let out = f(&conn);
            lock(&pool.idle).push(conn);
            out
        })
        .await
        .map_err(E::from)?
    }
}

impl Pool {
    fn open(&self) -> DbResult<Connection> {
        match &self.path {
            Some(path) => open_reader(path, self.busy_timeout),
            None => Err(DbError::Sqlite("reader of an in-memory database".into())),
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

/// Штатная остановка: статистика планировщика и WAL в основной файл.
pub fn close(conn: &Connection) -> DbResult<()> {
    conn.execute_batch("PRAGMA optimize; PRAGMA wal_checkpoint(PASSIVE);")?;
    Ok(())
}

/// PRAGMA каждого соединения (`10 §7`).
fn prepare(conn: &Connection, busy: Duration) -> DbResult<()> {
    conn.busy_timeout(busy)?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    math::install(conn)?;
    super::stats::install_day(conn)
}
