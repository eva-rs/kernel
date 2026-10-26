//! Ошибка БД и её подъём в `Aux::Db`: `protocol` не знает `rusqlite`.

use eva::data;

use eva_protocol::errors::{Aux, Generic};

pub type DbResult<T> = Result<T, DbError>;

#[data(error)]
pub enum DbError {
    /// Нарушен CHECK, UNIQUE, FK или сработал `RAISE(ABORT)` триггера.
    #[display("constraint: {_0}")]
    Constraint(String),
    /// Строка не разобралась в тип протокола: база испорчена или схема новее.
    #[display("corrupt row: {_0}")]
    Corrupt(String),
    #[display("sqlite: {_0}")]
    Sqlite(String),
    /// Блокирующая задача запроса упала.
    #[display("db task failed: {_0}")]
    Task(String),
}

impl DbError {
    pub fn corrupt(what: impl std::fmt::Display) -> Self {
        Self::Corrupt(what.to_string())
    }
}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        match &e {
            rusqlite::Error::SqliteFailure(f, _)
                if f.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Self::Constraint(e.to_string())
            }
            rusqlite::Error::FromSqlConversionFailure(..)
            | rusqlite::Error::InvalidColumnType(..)
            | rusqlite::Error::IntegralValueOutOfRange(..) => Self::Corrupt(e.to_string()),
            _ => Self::Sqlite(e.to_string()),
        }
    }
}

impl From<tokio::task::JoinError> for DbError {
    fn from(e: tokio::task::JoinError) -> Self {
        Self::Task(e.to_string())
    }
}

impl From<DbError> for Aux {
    fn from(e: DbError) -> Self {
        Self::Db(e.to_string())
    }
}

impl<E> From<DbError> for Generic<E> {
    fn from(e: DbError) -> Self {
        Self::Aux(e.into())
    }
}
