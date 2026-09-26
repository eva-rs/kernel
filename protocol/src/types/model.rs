//! Декларации провайдеров и моделей: единственный источник реестра моделей.

use eva::data;

use crate::types::{Effort, ModelId};

/// Уровень провайдера.
#[data(copy)]
#[derive(Default)]
pub struct ProviderDecl {
    /// Инференс на машине ядра: только такому эмбеддеру уходит private-текст.
    #[serde(default)]
    pub local: bool,
    /// Провайдер умеет `count_tokens`; иначе ядро считает калиброванным ratio.
    #[serde(default)]
    pub count_tokens: bool,
}

#[data]
pub struct ModelDecl {
    pub name: ModelId,
    pub context_window: u32,
    pub max_output: u32,
    #[serde(default)]
    pub caps: ModelCaps,
    pub price: Option<Price>,
    pub embed: Option<EmbedDecl>,
}

/// Нативные формы модели; интерпретация и fallback каждого флага — `01 §3`.
#[data]
#[derive(Default)]
#[serde(default)]
pub struct ModelCaps {
    pub vision: bool,
    pub image_out: bool,
    pub thinking: bool,
    pub effort_levels: Vec<Effort>,
    pub structured_output: bool,
    pub system_in_messages: bool,
    pub tool_changes: bool,
    pub clear_at: bool,
    pub effort_in_messages: bool,
    pub context_editing: bool,
    pub server_compaction: bool,
    pub thinking_binding: bool,
    /// 0 — кэша нет.
    pub cache_breakpoints: u8,
    pub cache_keep_alive: bool,
    /// Минимальный кэшируемый префикс в токенах; 0 — неизвестен.
    pub cache_min_prefix: u32,
}

/// Цена за MTok в валюте `llm.currency`.
#[data(copy)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write_short: f64,
    pub cache_write_long: f64,
}

#[data(copy)]
pub struct EmbedDecl {
    pub dims: u32,
    pub max_input_tokens: u32,
}

/// Строка `models.list`: декларация плюс имя провайдера из `llm.providers`.
#[data]
pub struct ModelEntry {
    /// Под `flatten` не класть `Timestamp`: буфер serde не читает его из msgpack.
    #[serde(flatten)]
    pub decl: ModelDecl,
    pub provider: String,
}
