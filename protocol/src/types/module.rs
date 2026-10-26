//! Модули soul.

use std::{
    borrow::Cow,
    fmt,
    net::Ipv6Addr,
    path::{Path, PathBuf},
    str::FromStr,
};

use eva::{
    _priv::schemars::{JsonSchema, Schema, SchemaGenerator, json_schema},
    data,
    time::Timestamp,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::types::ModuleId;

/// Вид модуля следует из соединения (`06 §2`): клиент звонит в soul с
/// токеном, а сервису soul звонит сам по реестру.
#[data(copy, ord, display(name))]
pub enum ModuleKind {
    Client,
    Service,
}

#[data(copy, ord)]
pub enum State {
    Connected,
    Offline,
    Forgotten,
}

#[data]
pub struct Module {
    pub name: ModuleId,
    /// `service` — запись реестра, `client` — живой токен, `None` — ни того, ни другого.
    pub kind: Option<ModuleKind>,
    pub state: State,
    pub version: String,
    pub proto_version: String,
    /// Последний переход состояния.
    pub since: Timestamp,
    /// Объявленные capability: `provider`, `tools`, `events`, `hooks`, `ask`.
    pub capabilities: Vec<String>,
    /// Только у записи реестра сервисов.
    pub dial: Option<Dial>,
}

/// Дозвон записи реестра (`06 §2.1`).
#[data]
pub struct Dial {
    pub address: ServiceAddress,
    /// Запись из `modules.services`: `modules.remove` её не снимет.
    pub config: bool,
    /// Последний провал до следующего `welcome`; живёт только в памяти soul.
    pub error: Option<String>,
}

/// Секрет PSK записи реестра (`06 §2.1`); `Debug` его не печатает.
#[data(not(Debug), ord)]
#[serde(transparent)]
pub struct ServiceKey(pub String);

impl ServiceKey {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ServiceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ServiceKey(***)")
    }
}

impl From<String> for ServiceKey {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ServiceKey {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Куда soul звонит сервисному модулю (`06 §1`): `uds:///<абсолютный путь>`
/// либо `tcp://<host>:<port>`; на проводе и в конфиге — эта строка.
#[data(ord, not(serde, JsonSchema))]
#[derive(Hash)]
pub enum ServiceAddress {
    Uds(PathBuf),
    /// `host` — имя или IP без скобок; резолвится на каждой попытке.
    Tcp {
        host: String,
        port: u16,
    },
}

impl ServiceAddress {
    pub fn parse(s: &str) -> Result<Self, ParseAddress> {
        if let Some(path) = s.strip_prefix("uds://") {
            return if path.starts_with('/') {
                Ok(Self::Uds(PathBuf::from(path)))
            } else {
                Err(ParseAddress::new(
                    s,
                    "uds path must be absolute: uds:///<path>",
                ))
            };
        }

        let Some(authority) = s.strip_prefix("tcp://") else {
            return Err(ParseAddress::new(s, "scheme must be uds:// or tcp://"));
        };
        if authority.contains(['/', '?', '#', '@']) {
            return Err(ParseAddress::new(
                s,
                "tcp address takes no path, query, fragment or userinfo",
            ));
        }
        let Some((host, port)) = authority.rsplit_once(':') else {
            return Err(ParseAddress::new(s, "tcp address needs a port"));
        };
        let port = port
            .parse::<u16>()
            .ok()
            .filter(|&port| port != 0)
            .ok_or_else(|| ParseAddress::new(s, "port must be 1..=65535"))?;
        let host = match host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
            Some(v6) if v6.parse::<Ipv6Addr>().is_ok() => v6,
            Some(_) => {
                return Err(ParseAddress::new(
                    s,
                    "bracketed host must be an IPv6 address",
                ));
            }
            None if host.is_empty() => {
                return Err(ParseAddress::new(s, "tcp address needs a host"));
            }
            None if host.contains(':') || host.contains(char::is_whitespace) => {
                return Err(ParseAddress::new(
                    s,
                    "host must be a name, an IPv4 or a bracketed IPv6 address",
                ));
            }
            None => host,
        };

        Ok(Self::Tcp {
            host: host.to_owned(),
            port,
        })
    }

    /// Путь сокета у `uds`.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Uds(path) => Some(path),
            Self::Tcp { .. } => None,
        }
    }
}

impl fmt::Display for ServiceAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uds(path) => write!(f, "uds://{}", path.display()),
            Self::Tcp { host, port } if host.contains(':') => write!(f, "tcp://[{host}]:{port}"),
            Self::Tcp { host, port } => write!(f, "tcp://{host}:{port}"),
        }
    }
}

impl FromStr for ServiceAddress {
    type Err = ParseAddress;

    fn from_str(s: &str) -> Result<Self, ParseAddress> {
        Self::parse(s)
    }
}

#[data(
    error,
    not(serde, JsonSchema),
    display("invalid service address {input:?}: {reason}")
)]
pub struct ParseAddress {
    pub input: String,
    pub reason: String,
}

impl ParseAddress {
    fn new(input: &str, reason: &str) -> Self {
        Self {
            input: input.to_owned(),
            reason: reason.to_owned(),
        }
    }
}

impl Serialize for ServiceAddress {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ServiceAddress {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = Cow::<str>::deserialize(deserializer)?;
        Self::parse(&s).map_err(de::Error::custom)
    }
}

impl JsonSchema for ServiceAddress {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ServiceAddress")
    }

    fn schema_id() -> Cow<'static, str> {
        Cow::Borrowed(concat!(module_path!(), "::ServiceAddress"))
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^(uds:///.*|tcp://[^/?#@]+:[0-9]+)$",
            "description": "Service module address: uds:///<absolute path> or tcp://<host>:<port>",
        })
    }
}
