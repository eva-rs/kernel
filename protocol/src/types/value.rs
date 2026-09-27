//! Значение без схемы: `kv` модуля и параметры `sqlite` (`06 §5`).

use std::{borrow::Cow, fmt};

use eva::{
    _priv::schemars::{JsonSchema, Schema, SchemaGenerator, json_schema},
    data,
};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, MapAccess, SeqAccess, Visitor},
};

use crate::types::Bytes;

/// Любое значение msgpack, включая `bin`: `serde_json::Value` байтов не держит,
/// и BLOB модуля нельзя было бы записать обратно. В JSON байты — base64-строка
/// и назад читаются как `String`.
#[data(not(serde, schemars))]
pub enum Value {
    Null,
    Bool(bool),
    Integer(i64),
    /// Только сверх `i64::MAX`: меньшие целые читаются как `Integer`.
    Unsigned(u64),
    Float(f64),
    String(String),
    Bytes(Bytes),
    Array(Vec<Value>),
    /// Порядок пар сохраняется; ключ msgpack — не обязательно строка.
    Map(Vec<(Value, Value)>),
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(b) => serializer.serialize_bool(*b),
            Self::Integer(i) => serializer.serialize_i64(*i),
            Self::Unsigned(u) => serializer.serialize_u64(*u),
            Self::Float(f) => serializer.serialize_f64(*f),
            Self::String(s) => serializer.serialize_str(s),
            Self::Bytes(b) => b.serialize(serializer),
            Self::Array(items) => serializer.collect_seq(items),
            Self::Map(pairs) => serializer.collect_map(pairs.iter().map(|(k, v)| (k, v))),
        }
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ValueVisitor)
    }
}

struct ValueVisitor;

impl<'de> Visitor<'de> for ValueVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any msgpack value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Integer(v))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(Value::from(v))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        Ok(Value::Float(v))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }

    fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Value, E> {
        Ok(Value::Bytes(Bytes::from(v)))
    }

    fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Value, E> {
        Ok(Value::Bytes(Bytes(v)))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(4096));
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut pairs = Vec::with_capacity(map.size_hint().unwrap_or(0).min(4096));
        while let Some(pair) = map.next_entry()? {
            pairs.push(pair);
        }
        Ok(Value::Map(pairs))
    }
}

impl JsonSchema for Value {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Value")
    }

    fn schema_id() -> Cow<'static, str> {
        Cow::Borrowed(concat!(module_path!(), "::Value"))
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "Any msgpack value; bytes are msgpack `bin` on the wire and base64 in JSON",
        })
    }
}

impl From<u64> for Value {
    fn from(v: u64) -> Self {
        i64::try_from(v).map_or(Self::Unsigned(v), Self::Integer)
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Self::Integer(v)
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Self::Float(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Self::String(v.to_owned())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Self::String(v)
    }
}

impl From<Bytes> for Value {
    fn from(v: Bytes) -> Self {
        Self::Bytes(v)
    }
}

impl From<serde_json::Value> for Value {
    fn from(v: serde_json::Value) -> Self {
        use serde_json::Value as Json;

        match v {
            Json::Null => Self::Null,
            Json::Bool(b) => Self::Bool(b),
            Json::Number(n) => match (n.as_i64(), n.as_u64()) {
                (Some(i), _) => Self::Integer(i),
                (None, Some(u)) => Self::Unsigned(u),
                (None, None) => Self::Float(n.as_f64().unwrap_or(f64::NAN)),
            },
            Json::String(s) => Self::String(s),
            Json::Array(items) => Self::Array(items.into_iter().map(Self::from).collect()),
            Json::Object(map) => Self::Map(
                map.into_iter()
                    .map(|(k, v)| (Self::String(k), Self::from(v)))
                    .collect(),
            ),
        }
    }
}
