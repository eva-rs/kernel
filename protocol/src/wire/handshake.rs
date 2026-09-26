//! Handshake (`06 §2`): `request hello` первым кадром, `response welcome`.

use std::{borrow::Cow, collections::BTreeMap, fmt, str::FromStr};

use eva::{
    _priv::schemars::{JsonSchema, Schema, SchemaGenerator, json_schema},
    data,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::{
    types::{ModelId, ModuleId, SessionId, ToolId, module::TokenKind},
    wire::{Capabilities, ErrorCode},
};

#[data]
pub struct Hello {
    /// Имя модуля — его идентичность и скоуп `fs`/`sqlite`/`kv`.
    pub module: ModuleId,
    /// Версия модуля, для `modules.list`.
    pub version: String,
    pub proto_version: ProtoVersion,
    pub token: Token,
    #[serde(default)]
    pub capabilities: Capabilities,
    /// Тот же процесс, что и прошлая сессия: переносит идемпотентность и незавершённые `tools.call`.
    pub resume: Option<Resume>,
}

#[data]
pub struct Resume {
    pub session_id: SessionId,
}

/// Токен модуля; `Debug` его не печатает.
#[data(not(Debug), ord)]
#[serde(transparent)]
pub struct Token(pub String);

impl Token {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(***)")
    }
}

impl From<String> for Token {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Token {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[data]
pub struct Welcome {
    pub session_id: SessionId,
    pub kernel_version: String,
    pub proto_version: ProtoVersion,
    /// Бит токена: по нему модуль строит свой `Caller`, подменить его нельзя.
    pub kind: TokenKind,
    /// Неймспейсы операций, которые ядро обслуживает по RPC.
    #[serde(default)]
    pub services: Vec<String>,
    /// `declared_path → tool_id`: модели не виден, модулю — для статистики и логов.
    #[serde(default)]
    pub tool_ids: BTreeMap<String, ToolId>,
    /// Принятые модели провайдера; отклонённые — с причиной в `warnings`.
    #[serde(default)]
    pub models_accepted: Vec<ModelId>,
    #[serde(default)]
    pub warnings: Vec<Warning>,
}

/// Отклонённая часть `hello`; сессия при этом жива.
#[data]
pub struct Warning {
    pub code: ErrorCode,
    /// Что отклонено: capability, путь тула, модель.
    pub subject: String,
    pub message: String,
}

/// Params `notify ping`.
#[data(copy)]
#[derive(Default)]
pub struct Ping {}

/// `MAJOR.MINOR`: MINOR — только добавления полей, вариантов и методов.
#[data(copy, ord, not(serde, JsonSchema), display("{major}.{minor}"))]
#[derive(Hash)]
pub struct ProtoVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtoVersion {
    /// Версия крейта — версия протокола.
    pub const CURRENT: Self = Self {
        major: parse_u16(env!("CARGO_PKG_VERSION_MAJOR")),
        minor: parse_u16(env!("CARGO_PKG_VERSION_MINOR")),
    };

    pub fn parse(s: &str) -> Option<Self> {
        let (major, minor) = s.split_once('.')?;
        Some(Self {
            major: major.parse().ok()?,
            minor: minor.parse().ok()?,
        })
    }
}

const fn parse_u16(s: &str) -> u16 {
    let bytes = s.as_bytes();
    let mut value: u16 = 0;
    let mut i = 0;
    while i < bytes.len() {
        assert!(bytes[i].is_ascii_digit(), "crate version is not numeric");
        value = value * 10 + (bytes[i] - b'0') as u16;
        i += 1;
    }
    value
}

impl FromStr for ProtoVersion {
    type Err = ParseVersion;

    fn from_str(s: &str) -> Result<Self, ParseVersion> {
        Self::parse(s).ok_or(ParseVersion)
    }
}

#[data(
    copy,
    not(serde, JsonSchema),
    display("protocol version must be MAJOR.MINOR"),
    error
)]
pub struct ParseVersion;

impl Serialize for ProtoVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ProtoVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = Cow::<str>::deserialize(deserializer)?;
        Self::parse(&s)
            .ok_or_else(|| de::Error::invalid_value(de::Unexpected::Str(&s), &"MAJOR.MINOR"))
    }
}

impl JsonSchema for ProtoVersion {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ProtoVersion")
    }

    fn schema_id() -> Cow<'static, str> {
        Cow::Borrowed(concat!(module_path!(), "::ProtoVersion"))
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^[0-9]+\\.[0-9]+$",
            "description": "Protocol version MAJOR.MINOR",
        })
    }
}
