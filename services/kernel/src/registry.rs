//! Реестр моделей (`01 §8.2`): объединение `Provider::models()`
//! сконфигурированных провайдеров; конфиг лишь фильтрует.

use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

use eva_core::Provider;
use eva_protocol::types::{
    ModelId,
    model::{ModelDecl, ModelEntry, ProviderDecl},
};

use crate::config::{LlmConfig, llm::ProviderEntry};

/// Снимок, который читают без блокировки на время запроса, а меняют целиком.
pub struct Snapshot<T>(RwLock<Arc<T>>);

impl<T> Snapshot<T> {
    pub fn new(value: T) -> Self {
        Self(RwLock::new(Arc::new(value)))
    }

    pub fn get(&self) -> Arc<T> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn replace(&self, value: T) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(value);
    }
}

/// Модель не принята в реестр.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Не прошла allow-list провайдера или `llm.models`.
    NotAllowed,
    /// Имя уже обслуживает другой провайдер: занятое не перехватывается.
    Taken { by: String },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAllowed => f.write_str("the model is not allowed by llm config"),
            Self::Taken { by } => write!(f, "the model is already declared by provider {by}"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Models {
    models: BTreeMap<ModelId, ModelEntry>,
    providers: BTreeMap<String, ProviderDecl>,
}

impl Models {
    /// Модели провайдеров по порядку имён: первая декларация имени побеждает.
    pub fn build<P: Provider>(providers: &BTreeMap<String, P>, llm: &LlmConfig) -> Self {
        let mut this = Self::default();
        for (name, provider) in providers {
            this.add(
                name,
                provider.decl(),
                provider.models(),
                allow_list(llm, name),
                llm,
            );
        }
        this
    }

    /// Модели одного провайдера; `allow` — его allow-list из конфига.
    pub fn add(
        &mut self,
        provider: &str,
        decl: ProviderDecl,
        models: Vec<ModelDecl>,
        allow: Option<&[ModelId]>,
        llm: &LlmConfig,
    ) {
        self.providers.insert(provider.to_owned(), decl);
        for model in models {
            if let Err(refusal) = self.admit(&model.name, allow, llm) {
                log::warn!(model:% = model.name, provider, reason:% = refusal; "model is not registered");
                continue;
            }
            self.models.insert(
                model.name.clone(),
                ModelEntry {
                    decl: model,
                    provider: provider.to_owned(),
                },
            );
        }
    }

    /// Провайдер ушёл вместе с моделями; чужие записи не трогаются.
    pub fn remove_provider(&mut self, provider: &str) {
        self.providers.remove(provider);
        self.models.retain(|_, entry| entry.provider != provider);
    }

    /// Примет ли реестр модель `name` от провайдера с allow-list `allow`.
    pub fn admit(
        &self,
        name: &ModelId,
        allow: Option<&[ModelId]>,
        llm: &LlmConfig,
    ) -> Result<(), Refusal> {
        let allowed = allow.is_none_or(|a| a.contains(name))
            && llm.models.as_ref().is_none_or(|m| m.contains(name));
        if !allowed {
            return Err(Refusal::NotAllowed);
        }
        match self.models.get(name) {
            Some(taken) => Err(Refusal::Taken {
                by: taken.provider.clone(),
            }),
            None => Ok(()),
        }
    }

    pub fn get(&self, name: &ModelId) -> Option<&ModelEntry> {
        self.models.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ModelEntry> {
        self.models.values()
    }

    pub fn provider(&self, name: &str) -> Option<ProviderDecl> {
        self.providers.get(name).copied()
    }
}

/// Allow-list модуля запасной двери; у встроенных он внутри их конфига.
pub fn allow_list<'a>(llm: &'a LlmConfig, provider: &str) -> Option<&'a [ModelId]> {
    match llm.providers.get(provider) {
        Some(ProviderEntry::Module(module)) => module.allow.as_deref(),
        _ => None,
    }
}
