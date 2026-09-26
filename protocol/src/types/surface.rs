//! Surface — лицо Евы: канал, в котором живут чаты.

use eva::{data, time::Timestamp};

use crate::types::{Effort, ModelId, ModuleId, SurfaceId, Thinking};

#[data]
pub struct Surface {
    pub id: SurfaceId,
    pub name: String,
    /// Модуль-создатель; `None` у builtin `local`/`system`.
    pub module: Option<ModuleId>,
    /// Prompt-блок канала, байт-стабильный.
    pub rules: Option<String>,
    /// Явный trust уровня surface; `None` — наследуется вниз как `false`.
    pub trusted: Option<bool>,
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
    pub thinking: Option<Thinking>,
    /// Доля окна модели, после которой сегмент компактится.
    pub compact_at: Option<f32>,
    pub builtin: Option<Builtin>,
    pub created_at: Timestamp,
}

/// Встроенные surface, засеянные ядром при первом старте.
#[data(copy, ord)]
pub enum Builtin {
    /// Клиенты хозяина: телефон, терминал, десктоп.
    Local,
    /// Крон, сон, уведомления.
    System,
}
