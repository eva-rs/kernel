//! Компонент `db` (`10`): соединения, миграции, сид и весь SQL ядра.
//!
//! SQL живёт только здесь, по файлу на домен. Запросы домена — синхронные
//! методы структуры над `&Connection` (`conn.surfaces().get(..)`), поэтому
//! одни и те же работают на читателе и внутри транзакции писателя:
//!
//! ```ignore
//! let db = db_conn.acquire(shared).await?;
//! let row = db.run(move |c| c.surfaces().get(&id)).await?;
//! shared.db().write(move |tx| tx.surfaces().save(&row)).await?;
//! ```

use std::future::Future;

pub use rusqlite::{Connection, Transaction as Tx};

pub use self::{
    error::{DbError, DbResult},
    sqlite::{Reader, Sqlite},
};

pub mod error;
pub mod math;
pub mod migrate;
pub mod quota_vfs;
pub mod seed;
pub mod sql;
pub mod sqlite;

pub mod agents;
pub mod blobs;
pub mod chats;
pub mod cron;
pub mod folders;
pub mod gc;
pub mod hands_grants;
pub mod kv;
pub mod memory;
pub mod meta;
pub mod modules;
pub mod notes;
pub mod persons;
pub mod sleep;
pub mod stats;
pub mod surfaces;
pub mod tools;
pub mod turns;

/// База ядра: пул читателей и единственный писатель (`10 §7`). В тестах —
/// обёртка с инъекцией отказов поверх `Sqlite`.
pub trait Db: Send + Sync + 'static {
    /// Доступ к пулу читателей: соединение занято только на время `Reader::run`.
    fn reader(&self) -> impl Future<Output = DbResult<Reader>> + Send;

    /// `f` в транзакции `BEGIN IMMEDIATE` писателя: `Ok` — коммит, `Err` — откат.
    fn write<T, E, F>(&self, f: F) -> impl Future<Output = Result<T, E>> + Send
    where
        T: Send + 'static,
        E: From<DbError> + Send + 'static,
        F: FnOnce(&Tx<'_>) -> Result<T, E> + Send + 'static;

    /// Соединение писателя без транзакции: миграции управляют ими сами.
    fn exclusive<T, E, F>(&self, f: F) -> impl Future<Output = Result<T, E>> + Send
    where
        T: Send + 'static,
        E: From<DbError> + Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, E> + Send + 'static;
}

macro_rules! queries {
    ($($method:ident: $module:ident::$Ty:ident),* $(,)?) => {
        /// Запросы доменов поверх соединения: читатель или транзакция писателя.
        pub trait Queries {
            $(fn $method(&self) -> $module::$Ty<'_>;)*
        }

        impl Queries for Connection {
            $(fn $method(&self) -> $module::$Ty<'_> {
                $module::$Ty(self)
            })*
        }
    };
}

queries! {
    agents: agents::Agents,
    blobs: blobs::Blobs,
    chats: chats::Chats,
    cron: cron::Cron,
    folders: folders::Folders,
    gc: gc::Gc,
    hands_grants: hands_grants::HandsGrants,
    kv: kv::Kv,
    memory: memory::Memory,
    meta: meta::Meta,
    modules: modules::Modules,
    notes: notes::Notes,
    persons: persons::Persons,
    sleep: sleep::Sleep,
    stats: stats::Stats,
    surfaces: surfaces::Surfaces,
    tools: tools::Tools,
    turns: turns::Turns,
}
