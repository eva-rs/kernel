//! Секция `llm` (`01 §8.2`), компонент `inference`. Реестр моделей строится
//! из `Provider::models()`, не из конфига; здесь — какие провайдеры
//! включить и глобальные уровни цепочки `05 §2.1`.

use std::{collections::BTreeMap, time::Duration};

use eva::{data, time::HumanDuration};
use serde::{Deserialize, Deserializer, de};
use serde_json::Value;

use eva_protocol::types::{Effort, ModelId, Thinking};
use eva_providers::ProviderConfig;

/// Секция без дефолта: `providers` и `default_model` обязательны.
#[data]
pub struct LlmConfig {
    pub providers: BTreeMap<String, ProviderEntry>,
    pub default_model: ModelId,
    #[serde(default)]
    pub default_effort: Effort,
    #[serde(default)]
    pub default_thinking: Thinking,
    /// Глобальный allow-list поверх деклараций; `None` — принять все.
    #[serde(default)]
    pub models: Option<Vec<ModelId>>,
    #[serde(default)]
    pub roles: Roles,
    #[serde(default)]
    pub fallback: Vec<ModelId>,
    #[serde(default = "default_retries")]
    pub retries: u32,
    #[serde(default = "default_retry_backoff")]
    pub retry_backoff: HumanDuration,
    #[serde(default = "default_provider_wait")]
    pub provider_wait: HumanDuration,
    #[serde(default = "default_currency")]
    pub currency: String,
}

impl LlmConfig {
    /// Минимальная секция: один провайдер и модель по умолчанию.
    pub fn new(providers: BTreeMap<String, ProviderEntry>, default_model: ModelId) -> Self {
        Self {
            providers,
            default_model,
            default_effort: Effort::default(),
            default_thinking: Thinking::default(),
            models: None,
            roles: Roles::default(),
            fallback: Vec::new(),
            retries: default_retries(),
            retry_backoff: default_retry_backoff(),
            provider_wait: default_provider_wait(),
            currency: default_currency(),
        }
    }

    /// Модуль назван запасной дверью `provider` (`06 §3`).
    pub fn names_module(&self, module: &str) -> bool {
        matches!(self.providers.get(module), Some(ProviderEntry::Module(_)))
    }
}

fn default_retries() -> u32 {
    3
}

fn default_retry_backoff() -> HumanDuration {
    HumanDuration(Duration::from_secs(1))
}

fn default_provider_wait() -> HumanDuration {
    HumanDuration(Duration::from_secs(120))
}

fn default_currency() -> String {
    String::from("USD")
}

/// Модели служебных ролей; `None` — правило из `01 §8.2`.
#[data]
#[derive(Default)]
#[serde(default)]
pub struct Roles {
    pub summary: Option<ModelId>,
    pub title: Option<ModelId>,
    pub cheap: Option<ModelId>,
    pub smartest: Option<ModelId>,
}

/// `llm.providers.<name>`: `kind: module` разбирает ядро, прочие `kind` —
/// vendor-строки, их разбирает `eva-providers` (I14).
#[data(not(Deserialize))]
#[serde(untagged)]
pub enum ProviderEntry {
    Module(ModuleProvider),
    Builtin(ProviderConfig),
}

/// Выбор варианта по `kind`, а не перебором: иначе опечатка в настройках
/// провайдера давала бы безликое «no variant matched».
impl<'de> Deserialize<'de> for ProviderEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let entry = if value.get("kind").and_then(Value::as_str) == Some("module") {
            serde_json::from_value(value).map(Self::Module)
        } else {
            serde_json::from_value(value).map(Self::Builtin)
        };
        entry.map_err(de::Error::custom)
    }
}

/// Запасная дверь: провайдер — модуль с capability `provider` и этим именем.
#[data]
pub struct ModuleProvider {
    pub kind: ModuleKind,
    /// Allow-list моделей модуля; `None` — все объявленные.
    #[serde(default)]
    pub allow: Option<Vec<ModelId>>,
}

#[data(copy)]
pub enum ModuleKind {
    Module,
}
