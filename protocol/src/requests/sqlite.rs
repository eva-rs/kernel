//! SQLite-файл модуля. `ATTACH` — `Aux::Forbidden`, перебор строк — `Aux::Quota`.

use std::{borrow::Cow, fmt};

use eva::{
    _priv::schemars::{JsonSchema, Schema, SchemaGenerator, json_schema},
    data,
};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, Visitor},
};

use crate::{
    errors,
    types::{Bytes, Value},
};

#[data]
pub struct Statement {
    pub sql: String,
    /// `?N` по порядку: `Bytes` — BLOB, `Bool` — 0/1, массив и карта — JSON-текст.
    #[serde(default)]
    pub params: Vec<Value>,
}

/// Ячейка выдачи `query` — класс хранения SQLite. BLOB на проводе — msgpack
/// `bin`, не массив чисел; в JSON он base64-строка и назад читается как `Text`.
#[data(not(serde, schemars))]
pub enum Cell {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Bytes),
}

impl Serialize for Cell {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Integer(i) => serializer.serialize_i64(*i),
            Self::Real(f) => serializer.serialize_f64(*f),
            Self::Text(s) => serializer.serialize_str(s),
            Self::Blob(b) => b.serialize(serializer),
        }
    }
}

/// Не `untagged`: строковый визитор serde принимает UTF-8 байты, и BLOB вернулся бы `Text`.
impl<'de> Deserialize<'de> for Cell {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(CellVisitor)
    }
}

struct CellVisitor;

impl<'de> Visitor<'de> for CellVisitor {
    type Value = Cell;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("null, an integer, a float, a string or bytes")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Cell, E> {
        Ok(Cell::Null)
    }

    fn visit_none<E: de::Error>(self) -> Result<Cell, E> {
        Ok(Cell::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Cell, D::Error> {
        deserializer.deserialize_any(self)
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Cell, E> {
        Ok(Cell::Integer(v))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Cell, E> {
        i64::try_from(v)
            .map(Cell::Integer)
            .map_err(|_| E::invalid_value(de::Unexpected::Unsigned(v), &"an SQLite INTEGER"))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Cell, E> {
        Ok(Cell::Real(v))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Cell, E> {
        Ok(Cell::Text(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Cell, E> {
        Ok(Cell::Text(v))
    }

    fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Cell, E> {
        Ok(Cell::Blob(Bytes::from(v)))
    }

    fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Cell, E> {
        Ok(Cell::Blob(Bytes(v)))
    }
}

impl JsonSchema for Cell {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Cell")
    }

    fn schema_id() -> Cow<'static, str> {
        Cow::Borrowed(concat!(module_path!(), "::Cell"))
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let blob = generator.subschema_for::<Bytes>();
        json_schema!({
            "description": "An SQLite value: NULL, INTEGER, REAL, TEXT or BLOB",
            "anyOf": [
                {"type": "null"},
                {"type": "integer", "format": "int64"},
                {"type": "number"},
                {"type": "string"},
                blob,
            ],
        })
    }
}

pub mod exec {
    use super::*;

    #[data]
    pub struct Args {
        #[serde(flatten)]
        pub statement: Statement,
    }

    #[data]
    pub struct Ok {
        pub changes: u64,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        Sql(#[from] errors::sqlite::Sql),
    }

    code!(Err => [Sql]);
}

pub mod query {
    use super::*;

    #[data]
    pub struct Args {
        #[serde(flatten)]
        pub statement: Statement,
    }

    #[data]
    pub struct Ok {
        pub columns: Vec<String>,
        pub rows: Vec<Vec<Cell>>,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        Sql(#[from] errors::sqlite::Sql),
    }

    code!(Err => [Sql]);
}

pub mod tx {
    use super::*;

    /// Одна транзакция: всё или ничего.
    #[data]
    pub struct Args {
        pub statements: Vec<Statement>,
    }

    #[data]
    pub struct Ok {
        /// По оператору.
        pub changes: Vec<u64>,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        Sql(#[from] errors::sqlite::Sql),
    }

    code!(Err => [Sql]);
}
