//! Секция `chats` (`05 §10`): ключи, нужные M1.

use eva::data;

use eva_protocol::types::{ModuleId, SurfaceId, chat};

#[data]
#[serde(default)]
pub struct ChatsConfig {
    /// Kind чата `chats.create` без `external_id`; ingest решает по актору.
    pub default_kind: chat::Kind,
    pub persons: PersonsConfig,
}

impl Default for ChatsConfig {
    fn default() -> Self {
        Self {
            default_kind: chat::Kind::Ephemeral,
            persons: PersonsConfig::default(),
        }
    }
}

#[data]
#[serde(default)]
pub struct PersonsConfig {
    /// Адреса хозяина; пусто — master-туров нет, только `p0`.
    pub master: Vec<MasterIdentity>,
    /// Имя личности хозяина при сиде.
    pub master_name: String,
}

impl Default for PersonsConfig {
    fn default() -> Self {
        Self {
            master: Vec::new(),
            master_name: String::from("Master"),
        }
    }
}

/// Адрес хозяина на поверхности.
#[data]
pub struct MasterIdentity {
    pub surface: SurfaceId,
    pub external_id: String,
    /// Единственный модуль, которому можно создать этот surface; `None` — любой сервис.
    #[serde(default)]
    pub module: Option<ModuleId>,
}
