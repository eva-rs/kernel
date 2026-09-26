//! Декларации тулов и их видимость.

use eva::{data, time::HumanDuration};
use serde_json::Value;

use crate::types::{BlobId, ToolId};

/// Требование тула к контексту тура; предикаты задают главы-владельцы.
#[data(copy, ord, display(name))]
#[derive(Hash)]
pub enum Need {
    Hands,
    Lead,
    Team,
    Agent,
    Work,
    Private,
    AskUi,
    IndexNotes,
    Sleep,
    Embedder,
}

/// Форма результата: `windowed` ядро режет хвост, `uncapped` тул режет сам.
#[data(copy, ord)]
#[derive(Default)]
pub enum ResultForm {
    #[default]
    Windowed,
    Uncapped,
}

#[data]
pub struct ToolDecl {
    /// Путь через точку, ≥2 сегментов; последний — имя тула.
    pub path: String,
    pub description: String,
    /// JSON Schema входа, draft 2020-12.
    pub input: Value,
    #[serde(default)]
    pub master_only: bool,
    #[serde(default)]
    pub trusted_only: bool,
    #[serde(default)]
    pub needs: Vec<Need>,
    #[serde(default)]
    pub ends_turn: bool,
    #[serde(default)]
    pub parallel_safe: bool,
    #[serde(default)]
    pub waits_on_cancel: bool,
    #[serde(default)]
    pub result: ResultForm,
    pub timeout: Option<HumanDuration>,
    #[serde(default)]
    pub idempotent: bool,
}

#[data]
pub struct GroupDecl {
    pub path: String,
    pub about: String,
}

/// Причина, по которой тул недоступен туру; строки байт-стабильны.
#[data(copy, ord)]
pub enum Forbid {
    ModuleOffline,
    NameCollision,
    NotInWhitelist,
    Override,
    MasterOnly,
    TrustedOnly,
    Needs(Need),
}

#[data(copy, ord)]
pub enum Exposure {
    Visible,
    Reserved,
    Forbidden(Forbid),
}

/// Строка `tools.catalog`.
#[data]
pub struct CatalogEntry {
    pub tool_id: ToolId,
    pub path: String,
    pub canonical: String,
    pub description: String,
    /// `about` группы тула.
    pub about: String,
    pub needs: Vec<Need>,
    pub master_only: bool,
    pub trusted_only: bool,
    /// Только при заданном `chat`.
    pub exposure: Option<Exposure>,
}

/// Кусок результата тула; картинка доезжает до модели картинкой.
#[data]
pub enum ToolContent {
    Text { text: String },
    Image { media_type: String, data: Vec<u8> },
    Blob { id: BlobId },
}
