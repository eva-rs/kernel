//! Колонки ↔ типы протокола (`10 §1`): время — INTEGER наносекунд, enum —
//! TEXT snake_case варианта, `Effort` — INTEGER 1..=4.

use std::num::NonZeroU64;

use eva::time::Timestamp;
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use eva_protocol::types::Effort;

/// `Timestamp` в INTEGER-колонке.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ts(pub Timestamp);

impl Ts {
    pub fn nanos(self) -> rusqlite::Result<i64> {
        i64::try_from(self.0.as_nanos().get())
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
    }
}

impl ToSql for Ts {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.nanos().map(ToSqlOutput::from)
    }
}

impl FromSql for Ts {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let nanos = i64::column_result(value)?;
        u64::try_from(nanos)
            .ok()
            .and_then(NonZeroU64::new)
            .map(|n| Self(Timestamp::from_nanos(n)))
            .ok_or(FromSqlError::OutOfRange(nanos))
    }
}

/// Unit-enum `#[data]` в TEXT-колонке под CHECK: строка — его serde-имя.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Text<T>(pub T);

impl<T: Serialize> ToSql for Text<T> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        match serde_json::to_value(&self.0) {
            Ok(Value::String(s)) => Ok(ToSqlOutput::from(s)),
            Ok(other) => Err(rusqlite::Error::ToSqlConversionFailure(
                format!("not a unit variant: {other}").into(),
            )),
            Err(e) => Err(rusqlite::Error::ToSqlConversionFailure(Box::new(e))),
        }
    }
}

impl<T: DeserializeOwned> FromSql for Text<T> {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        serde_json::from_value(Value::String(s.to_owned()))
            .map(Self)
            .map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}

/// `Effort` в INTEGER-колонке `CHECK (effort BETWEEN 1 AND 4)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level(pub Effort);

impl ToSql for Level {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.0.as_u8()))
    }
}

impl FromSql for Level {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let raw = i64::column_result(value)?;
        u8::try_from(raw)
            .ok()
            .and_then(Effort::from_u8)
            .map(Self)
            .ok_or(FromSqlError::OutOfRange(raw))
    }
}
