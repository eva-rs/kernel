//! Встроенные провайдеры: конфиг под тегом `kind` и диспетчер — vendor-строка
//! `kind` разбирается здесь, а не в ядре (I14).

#![cfg_attr(
    not(any(feature = "openai", feature = "mock")),
    allow(unused_variables, unreachable_code)
)]

use eva::data;

use eva_core::{GenSink, Provider};
use eva_protocol::types::{
    generate::{EmbedRequest, EmbedResponse, GenerateRequest, ProviderError},
    model::{ModelDecl, ProviderDecl},
};

#[cfg(feature = "mock")]
pub use eva_provider_mock as mock;
#[cfg(feature = "openai")]
pub use eva_provider_openai as openai;

/// `llm.providers.<name>` встроенного провайдера.
#[data(not(as_static_str))]
#[serde(tag = "kind")]
pub enum ProviderConfig {
    #[cfg(feature = "openai")]
    Openai(openai::Config),
    #[cfg(feature = "mock")]
    Mock(mock::Config),
}

/// Провайдер не собрался; `provider` — имя из `llm.providers`, ошибка ключа
/// записи читается путём: `llm.providers.<name>: api_key: …`.
#[data(error, display("llm.providers.{provider}: {reason}"))]
pub struct BuildError {
    pub provider: String,
    pub reason: String,
}

/// Собирает провайдер; `openai` при этом один раз читает каталог моделей.
pub async fn build(name: &str, config: ProviderConfig) -> Result<AnyProvider, BuildError> {
    let provider: AnyProvider = match config {
        #[cfg(feature = "openai")]
        ProviderConfig::Openai(config) => openai::OpenAiProvider::connect(config)
            .await
            .map(|p| AnyProvider::Openai(Box::new(p)))
            .map_err(|e| BuildError {
                provider: name.to_owned(),
                reason: e.reason,
            })?,
        #[cfg(feature = "mock")]
        ProviderConfig::Mock(config) => AnyProvider::Mock(mock::MockProvider::new(config)),
    };
    eva::logging::info!(provider = name, models = provider.models().len(); "provider ready");
    Ok(provider)
}

pub enum AnyProvider {
    #[cfg(feature = "openai")]
    Openai(Box<openai::OpenAiProvider>),
    #[cfg(feature = "mock")]
    Mock(mock::MockProvider),
}

impl AnyProvider {
    /// Дочитывает каталог моделей, недоступный при [`build`]; `true` — `models()`
    /// сменился и реестр моделей пора пересобрать.
    pub async fn settle(&self) -> bool {
        match *self {
            #[cfg(feature = "openai")]
            Self::Openai(ref p) => p.settle().await,
            #[cfg(feature = "mock")]
            Self::Mock(ref p) => p.settle().await,
        }
    }
}

impl Provider for AnyProvider {
    fn decl(&self) -> ProviderDecl {
        match *self {
            #[cfg(feature = "openai")]
            Self::Openai(ref p) => p.decl(),
            #[cfg(feature = "mock")]
            Self::Mock(ref p) => p.decl(),
        }
    }

    fn models(&self) -> Vec<ModelDecl> {
        match *self {
            #[cfg(feature = "openai")]
            Self::Openai(ref p) => p.models(),
            #[cfg(feature = "mock")]
            Self::Mock(ref p) => p.models(),
        }
    }

    async fn generate(&self, req: GenerateRequest, sink: GenSink) -> Result<(), ProviderError> {
        match *self {
            #[cfg(feature = "openai")]
            Self::Openai(ref p) => p.generate(req, sink).await,
            #[cfg(feature = "mock")]
            Self::Mock(ref p) => p.generate(req, sink).await,
        }
    }

    async fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse, ProviderError> {
        match *self {
            #[cfg(feature = "openai")]
            Self::Openai(ref p) => p.embed(req).await,
            #[cfg(feature = "mock")]
            Self::Mock(ref p) => p.embed(req).await,
        }
    }

    async fn count_tokens(&self, req: GenerateRequest) -> Result<u32, ProviderError> {
        match *self {
            #[cfg(feature = "openai")]
            Self::Openai(ref p) => p.count_tokens(req).await,
            #[cfg(feature = "mock")]
            Self::Mock(ref p) => p.count_tokens(req).await,
        }
    }
}
