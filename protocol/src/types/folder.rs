//! Папка внутри surface: уровень наследования trust и модели.

use std::fmt;

use eva::data;

use crate::types::{Effort, FolderId, ModelId, SurfaceId, Thinking};

#[data]
pub struct Folder {
    pub id: FolderId,
    pub surface_id: SurfaceId,
    pub name: String,
    /// `Unsorted`, `agents` и служебные папки `system` — не удаляются и не переименовываются.
    pub builtin: bool,
    pub trusted: Option<bool>,
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
    pub thinking: Option<Thinking>,
    pub compact_at: Option<f32>,
}

/// Папка по id или по имени внутри surface.
///
/// Untagged: на проводе `7` либо `{"surface": "x", "name": "work"}`.
#[data]
#[serde(untagged)]
pub enum Selector {
    Id(FolderId),
    Name { surface: SurfaceId, name: String },
}

impl From<FolderId> for Selector {
    fn from(value: FolderId) -> Self {
        Self::Id(value)
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Id(id) => id.fmt(f),
            Self::Name { surface, name } => write!(f, "{surface}/{name}"),
        }
    }
}
