//! Секция `chats` (`05 §10`): ключи, нужные M1 и поиску по чатам (`05 §7`).

use std::time::Duration;

use eva::{data, time::HumanDuration};

use eva_protocol::types::{ModuleId, SurfaceId, chat};

#[data]
#[serde(default)]
pub struct ChatsConfig {
    /// Kind чата `chats.create` без `external_id`; ingest решает по актору.
    pub default_kind: chat::Kind,
    pub persons: PersonsConfig,
    pub marker: MarkerConfig,
    pub search: SearchConfig,
    pub title: TitleConfig,
    pub history: HistoryConfig,
    pub project_docs: ProjectDocsConfig,
}

impl Default for ChatsConfig {
    fn default() -> Self {
        Self {
            default_kind: chat::Kind::Ephemeral,
            persons: PersonsConfig::default(),
            marker: MarkerConfig::default(),
            search: SearchConfig::default(),
            title: TitleConfig::default(),
            history: HistoryConfig::default(),
            project_docs: ProjectDocsConfig::default(),
        }
    }
}

impl ChatsConfig {
    pub fn check(&self) -> eyre::Result<()> {
        eyre::ensure!(
            self.history.read_tokens >= 128,
            "chats.history.read_tokens must be at least 128 to fit a continuation cursor"
        );
        Ok(())
    }
}

/// Бюджеты служебного запроса названия.
#[data]
#[serde(default)]
pub struct TitleConfig {
    pub input_tokens: u32,
    pub reasoning_headroom: u32,
    pub len_tokens: u32,
}

impl Default for TitleConfig {
    fn default() -> Self {
        Self {
            input_tokens: 512,
            reasoning_headroom: 512,
            len_tokens: 16,
        }
    }
}

/// Максимальное окно истории в ответе инструмента модели.
#[data]
#[serde(default)]
pub struct HistoryConfig {
    pub read_tokens: u32,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self { read_tokens: 4000 }
    }
}

/// Бюджет AGENTS.md в снимке контекста чата.
#[data]
#[serde(default)]
pub struct ProjectDocsConfig {
    pub max_tokens: u32,
}

impl Default for ProjectDocsConfig {
    fn default() -> Self {
        Self { max_tokens: 4000 }
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

/// Потолки `sanitize_inline` (`05 §4`), токенов.
#[data]
#[serde(default)]
pub struct MarkerConfig {
    /// Сниппет цели ответа в маркере.
    pub snippet_tokens: u32,
    /// Имя, handle, название в служебной строке.
    pub name_tokens: u32,
}

impl Default for MarkerConfig {
    fn default() -> Self {
        Self {
            snippet_tokens: 24,
            name_tokens: 12,
        }
    }
}

/// Поиск по чатам (`05 §7`).
#[data]
#[serde(default)]
pub struct SearchConfig {
    /// Кандидатов из каждого источника до RRF.
    pub candidates: u32,
    pub rrf_k: f64,
    /// Реплика длиннее — режется на чанки по границам строк.
    pub chunk_tokens: u32,
    /// Пауза перед пачкой индексации.
    pub index_delay: HumanDuration,
    /// Сниппет хита.
    pub snippet_tokens: u32,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            candidates: 50,
            rrf_k: 60.0,
            chunk_tokens: 512,
            index_delay: HumanDuration(Duration::from_secs(5)),
            snippet_tokens: 48,
        }
    }
}
