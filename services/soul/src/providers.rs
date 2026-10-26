//! Провайдеры soul (`11 §5`): `SoulProvider` — `Provider` реестра, будь то
//! скомпилированный крейт `providers/*` или модуль запасной двери.

use std::collections::BTreeMap;

use futures::future::try_join_all;

use eva_core::{GenSink, Provider};
use eva_protocol::types::{
    generate::{EmbedRequest, EmbedResponse, GenerateRequest, ProviderError},
    model::{ModelDecl, ProviderDecl},
};
use eva_providers::AnyProvider;
use eva_rpc::ModuleProvider;

use crate::config::{LlmConfig, llm::ProviderEntry};

/// Провайдер реестра моделей.
pub enum SoulProvider {
    /// Крейт `providers/*`, выбранный `kind` записи `llm.providers`.
    Builtin(AnyProvider),
    /// Модуль с capability `provider`, пока жива его сессия (`06 §3`).
    Module(ModuleProvider),
}

impl SoulProvider {
    /// Встроенные провайдеры из `llm.providers`, параллельно: сборка может
    /// читать каталог моделей по сети. `kind: module` подключается на
    /// handshake модуля.
    pub async fn from_config(llm: &LlmConfig) -> eyre::Result<BTreeMap<String, Self>> {
        let builds = llm
            .providers
            .iter()
            .filter_map(|(name, entry)| match entry {
                ProviderEntry::Builtin(cfg) => Some(async move {
                    let provider = eva_providers::build(name, cfg.clone()).await?;
                    eyre::Ok((name.clone(), Self::Builtin(provider)))
                }),
                ProviderEntry::Module(_) => None,
            });
        Ok(try_join_all(builds).await?.into_iter().collect())
    }
}

/// Каталог моделей, недоступный при сборке провайдера, дочитывается фоном.
pub trait Settle: Send + Sync {
    /// `true` — `models()` сменился и часть реестра пора пересобрать.
    fn settle(&self) -> impl Future<Output = bool> + Send;
}

impl Settle for SoulProvider {
    async fn settle(&self) -> bool {
        match self {
            Self::Builtin(p) => p.settle().await,
            Self::Module(_) => false,
        }
    }
}

impl From<AnyProvider> for SoulProvider {
    fn from(provider: AnyProvider) -> Self {
        Self::Builtin(provider)
    }
}

impl From<ModuleProvider> for SoulProvider {
    fn from(provider: ModuleProvider) -> Self {
        Self::Module(provider)
    }
}

impl Provider for SoulProvider {
    fn decl(&self) -> ProviderDecl {
        match self {
            Self::Builtin(p) => p.decl(),
            Self::Module(p) => p.decl(),
        }
    }

    fn models(&self) -> Vec<ModelDecl> {
        match self {
            Self::Builtin(p) => p.models(),
            Self::Module(p) => p.models(),
        }
    }

    async fn generate(&self, req: GenerateRequest, sink: GenSink) -> Result<(), ProviderError> {
        match self {
            Self::Builtin(p) => p.generate(req, sink).await,
            Self::Module(p) => p.generate(req, sink).await,
        }
    }

    async fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse, ProviderError> {
        match self {
            Self::Builtin(p) => p.embed(req).await,
            Self::Module(p) => p.embed(req).await,
        }
    }

    async fn count_tokens(&self, req: GenerateRequest) -> Result<u32, ProviderError> {
        match self {
            Self::Builtin(p) => p.count_tokens(req).await,
            Self::Module(p) => p.count_tokens(req).await,
        }
    }
}
