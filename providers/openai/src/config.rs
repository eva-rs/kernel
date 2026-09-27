//! `llm.providers.<name>` с `kind: openai`.

use std::{collections::BTreeMap, time::Duration};

use eva::{data, time::HumanDuration, utils::SecretString};

use eva_protocol::types::{
    Effort, ModelId,
    model::{EmbedDecl, Price},
};

pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

#[data]
#[serde(default)]
pub struct Config {
    /// Корень API без `/chat/completions`.
    pub base_url: String,
    /// `{plaintext: …}` или `{file: …}` (`eva::utils::SecretString`); файл — например `LoadCredential` systemd.
    pub api_key: Option<SecretString>,
    /// `http://`, `https://` или `socks5://`.
    pub proxy: Option<String>,
    /// Потолок подключения и тишины между чтениями, не всего ответа: длинная генерация не рвётся.
    pub timeout: HumanDuration,
    pub dialect: Dialect,
    /// Allow-list моделей; `None` — всё, что объявил сервер и `models`.
    pub allow: Option<Vec<ModelId>>,
    /// Поправки к `GET /models`; у сервера без описаний моделей — единственный источник.
    pub models: BTreeMap<ModelId, ModelOverride>,
    /// Заголовок `HTTP-Referer`: атрибуция приложения у OpenRouter.
    pub referer: Option<String>,
    /// Заголовок `X-Title`.
    pub title: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_url: OPENROUTER_BASE_URL.into(),
            api_key: None,
            proxy: None,
            timeout: HumanDuration(Duration::from_secs(300)),
            dialect: Dialect::default(),
            allow: None,
            models: BTreeMap::new(),
            referer: None,
            title: None,
        }
    }
}

#[data(copy, ord)]
#[derive(Default)]
pub enum Dialect {
    /// Расширения OpenRouter: `reasoning`, `reasoning_details`, `cache_control`, `usage.cost`.
    #[default]
    Openrouter,
    /// Голый Chat Completions: vLLM, llama-server, OpenAI.
    Plain,
}

/// Модель, которой нет в `GET /models`, обязана задать `context_window` (или `embed`).
#[data]
#[derive(Default)]
#[serde(default, deny_unknown_fields)]
pub struct ModelOverride {
    pub context_window: Option<u32>,
    pub max_output: Option<u32>,
    pub price: Option<Price>,
    pub embed: Option<EmbedDecl>,
    pub caps: CapsOverride,
}

/// Только флаги, которые Chat Completions умеет выразить; нативных форм
/// `tool_changes`/`clear_at`/`effort_in_messages`/`context_editing`/
/// `server_compaction`/`cache_keep_alive` у него нет — они всегда `false`.
#[data]
#[derive(Default)]
#[serde(default, deny_unknown_fields)]
pub struct CapsOverride {
    pub vision: Option<bool>,
    pub image_out: Option<bool>,
    pub thinking: Option<bool>,
    /// `max` в списке — запрос `xhigh`, иначе `max` едет как `high`.
    pub effort_levels: Option<Vec<Effort>>,
    pub structured_output: Option<bool>,
    pub system_in_messages: Option<bool>,
    pub thinking_binding: Option<bool>,
    /// Только диалект `openrouter`: у `plain` нет `cache_control`.
    pub cache_breakpoints: Option<u8>,
    pub cache_min_prefix: Option<u32>,
}
