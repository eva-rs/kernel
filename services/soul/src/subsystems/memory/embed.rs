//! Эмбеддер памяти и поиска (`03 §8`): провайдер из реестра по
//! `memory.embedder`, кэш векторов по `blake3(text)`, таймауты запросов,
//! воркер backfill. Факты о модели — из `ModelDecl.embed`, не из конфига.

use std::{sync::Arc, time::Duration};

use eva_core::Provider as _;
use eva_protocol::types::{
    ModelId,
    generate::{EmbedRequest, ProviderError},
};

use crate::{
    components::Components,
    db::{Db as _, DbError, Queries as _, stats::Metric},
    shared::{Registered, Shared},
};

pub struct Embedder<C: Components> {
    shared: Shared<C>,
    provider: Arc<Registered<C>>,
    name: String,
    model: ModelId,
    dims: u32,
    local: bool,
}

/// Отказ воркера: провайдер или база.
#[derive(Debug)]
pub enum Error {
    Provider(ProviderError),
    Db(DbError),
}

impl From<DbError> for Error {
    fn from(e: DbError) -> Self {
        Self::Db(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Provider(e) => write!(f, "provider: {}", e.raw),
            Self::Db(e) => write!(f, "{e}"),
        }
    }
}

impl<C: Components> Embedder<C> {
    /// `memory.embedder` → провайдер; `None` → первый по имени с
    /// `local ∧ embed-модель`; модель — `memory.embed.model` ?? первая его
    /// embed-модель. Без подходящей пары эмбеддера нет: recall и дедуп идут
    /// на FTS ∪ Jaccard.
    pub fn resolve(shared: &Shared<C>) -> Option<Self> {
        let cfg = &shared.cfg().memory;
        let models = shared.models();
        let pick = |provider: &str| -> Option<(ModelId, u32)> {
            match &cfg.embed.model {
                Some(model) => models
                    .get(model)
                    .filter(|entry| entry.provider == provider)
                    .and_then(|entry| entry.decl.embed.map(|e| (model.clone(), e.dims))),
                None => models
                    .iter()
                    .filter(|entry| entry.provider == provider)
                    .find_map(|entry| entry.decl.embed.map(|e| (entry.decl.name.clone(), e.dims))),
            }
        };
        let (name, model, dims) = match &cfg.embedder {
            Some(name) => {
                let Some((model, dims)) = pick(name) else {
                    log::debug!(provider = name.as_str(); "memory.embedder has no embed model in the registry");
                    return None;
                };
                (name.clone(), model, dims)
            }
            None => shared.providers().keys().find_map(|name| {
                let local = models.provider(name)?.local;
                let (model, dims) = pick(name)?;
                local.then_some((name.clone(), model, dims))
            })?,
        };
        let provider = shared.provider(&name)?;
        let local = models.provider(&name).is_some_and(|decl| decl.local);
        Some(Self {
            shared: shared.clone(),
            provider,
            name,
            model,
            dims,
            local,
        })
    }

    pub fn model(&self) -> &ModelId {
        &self.model
    }

    pub fn dims(&self) -> u32 {
        self.dims
    }

    /// Инференс на машине хозяина: только такому уходят private-строки и trusted-речь.
    pub fn local(&self) -> bool {
        self.local
    }

    pub fn provider_name(&self) -> &str {
        &self.name
    }

    /// Вектор одного текста с кэшем и таймаутом; `None` — не успели или
    /// провайдер отказал (вызывающий деградирует до FTS).
    pub async fn query(&self, text: &str, timeout: Duration) -> Option<Vec<f32>> {
        let hash = *blake3::hash(text.as_bytes()).as_bytes();
        let model = self.model.clone();
        let cached = match self.shared.db().reader().await {
            Ok(reader) => reader
                .run(move |c| c.memory().cached_vector(&hash, &model))
                .await
                .unwrap_or_else(|e: DbError| {
                    log::warn!(error:% = e; "embed cache read failed");
                    None
                }),
            Err(e) => {
                log::warn!(error:% = e; "embed cache reader unavailable");
                None
            }
        };
        if let Some(vector) = cached {
            return Some(vector);
        }
        let request = EmbedRequest {
            model: self.model.clone(),
            inputs: vec![text.to_owned()],
        };
        let vector = match tokio::time::timeout(timeout, self.provider.embed(request)).await {
            Ok(Ok(mut res)) if res.vectors.len() == 1 => res.vectors.pop()?,
            Ok(Ok(res)) => {
                log::warn!(model:% = self.model, vectors = res.vectors.len(); "embedder returned a wrong vector count");
                return None;
            }
            Ok(Err(e)) => {
                log::warn!(model:% = self.model, kind:? = e.kind, error = e.raw.as_str(); "embedder failed");
                return None;
            }
            Err(_) => {
                self.shared.bump(Metric::MemoryEmbedTimeouts, "").await;
                return None;
            }
        };
        let (model, now, cached) = (self.model.clone(), self.shared.now(), vector.clone());
        if let Err(e) = self
            .shared
            .db()
            .write(move |tx| tx.memory().cache_vector(&hash, &model, &cached, now))
            .await
        {
            log::warn!(error:% = e; "embed cache write failed");
        }
        Some(vector)
    }

    /// Пачка без кэша и таймаута — воркер.
    pub async fn embed(&self, inputs: Vec<String>) -> Result<Vec<Vec<f32>>, ProviderError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let expected = inputs.len();
        let res = self
            .provider
            .embed(EmbedRequest {
                model: self.model.clone(),
                inputs,
            })
            .await?;
        if res.vectors.len() != expected {
            return Err(ProviderError {
                kind: eva_protocol::types::generate::ProviderErrorKind::Other,
                http_status: None,
                raw: format!(
                    "embedder returned {} vectors for {expected} inputs",
                    res.vectors.len()
                ),
                retry_after: None,
            });
        }
        Ok(res.vectors)
    }

    /// Backfill памяти (`03 §8`): строки без вектора активной модели,
    /// private — только локальному эмбеддеру; сколько строк заполнено.
    pub async fn backfill(&self) -> Result<usize, Error> {
        let batch = self.shared.cfg().memory.embed.batch.max(1);
        let mut done = 0;
        loop {
            let (model, local) = (self.model.clone(), self.local);
            let queue = self
                .shared
                .db()
                .reader()
                .await?
                .run(move |c| c.memory().embed_queue(&model, local, batch))
                .await?;
            if queue.is_empty() {
                return Ok(done);
            }
            let texts: Vec<String> = queue.iter().map(|(_, text)| text.clone()).collect();
            let vectors = self.embed(texts).await.map_err(Error::Provider)?;
            let (model, now) = (self.model.clone(), self.shared.now());
            let count = queue.len();
            self.shared
                .db()
                .write(move |tx| {
                    for ((id, text), vector) in queue.iter().zip(&vectors) {
                        tx.memory().set_embedding(*id, &model, vector)?;
                        let hash = *blake3::hash(text.as_bytes()).as_bytes();
                        tx.memory().cache_vector(&hash, &model, vector, now)?;
                    }
                    Ok::<(), DbError>(())
                })
                .await?;
            done += count;
            if count < batch as usize {
                return Ok(done);
            }
        }
    }
}
