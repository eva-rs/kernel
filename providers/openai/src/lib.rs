//! OpenAI-совместимый Chat Completions за трейтом `Provider`: OpenRouter
//! (`openrouter`) и локальные серверы вроде vLLM и llama-server (`plain`).
//!
//! Процессный провайдер криптографии rustls ставит бинарь до
//! [`OpenAiProvider::connect`]: без него сборка HTTP-клиента паникует.

mod config;
mod error;
mod models;
mod request;
mod stream;

use std::{
    collections::BTreeMap,
    net::IpAddr,
    sync::{Arc, PoisonError, RwLock},
};

use eva::data;
use reqwest::{RequestBuilder, Url};
use serde_json::{Value, json};

use eva_core::{GenSink, Provider};
use eva_protocol::types::{
    ModelId,
    generate::{
        EmbedRequest, EmbedResponse, GenerateRequest, ProviderError, ProviderErrorKind, Usage,
    },
    model::{ModelDecl, ProviderDecl},
};

pub use config::{ApiKey, CapsOverride, Config, Dialect, ModelOverride, OPENROUTER_BASE_URL};
pub use error::classify;
pub use models::declare;
pub use request::{normalize_schema, render};
pub use stream::{Decoder, pump};

/// Провайдер не собрался: конфиг или первый запрос реестра.
#[data(error, display("{reason}"))]
pub struct BuildError {
    pub reason: String,
}

impl BuildError {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

pub struct OpenAiProvider {
    config: Config,
    base: Url,
    client: reqwest::Client,
    models: RwLock<Arc<BTreeMap<ModelId, ModelDecl>>>,
}

impl OpenAiProvider {
    /// Собирает клиент и реестр. `openrouter` без `GET /models` не стартует —
    /// каталог и есть источник окон и цен; `plain` живёт на поправках конфига.
    pub async fn connect(config: Config) -> Result<Self, BuildError> {
        let provider = Self::unlisted(config)?;
        match provider.fetch_listing().await {
            Ok(listing) => provider.install(Some(&listing)),
            Err(e) if provider.config.dialect == Dialect::Plain => {
                log::warn!(base_url = provider.base.as_str(), error:% = e; "models endpoint unavailable, config models only");
                provider.install(None);
            }
            Err(e) => return Err(BuildError::new(format!("GET /models: {e}"))),
        }
        Ok(provider)
    }

    /// Без сети: реестр из готового ответа `GET /models` (или только из конфига).
    pub fn from_listing(config: Config, listing: Option<&Value>) -> Result<Self, BuildError> {
        let provider = Self::unlisted(config)?;
        provider.install(listing);
        Ok(provider)
    }

    fn unlisted(config: Config) -> Result<Self, BuildError> {
        let base = Url::parse(config.base_url.trim_end_matches('/'))
            .map_err(|e| BuildError::new(format!("base_url: {e}")))?;
        let mut client = reqwest::Client::builder()
            .connect_timeout(*config.timeout)
            .read_timeout(*config.timeout);
        if let Some(proxy) = &config.proxy {
            let proxy = reqwest::Proxy::all(proxy.as_str())
                .map_err(|e| BuildError::new(format!("proxy: {e}")))?;
            client = client.proxy(proxy);
        }
        let client = client
            .build()
            .map_err(|e| BuildError::new(format!("http client: {e}")))?;
        Ok(Self {
            config,
            base,
            client,
            models: RwLock::default(),
        })
    }

    /// Перечитать `GET /models`; при ошибке реестр остаётся прежним.
    pub async fn refresh(&self) -> Result<(), ProviderError> {
        let listing = self.fetch_listing().await?;
        self.install(Some(&listing));
        Ok(())
    }

    fn install(&self, listing: Option<&Value>) {
        let declared = declare(listing, &self.config);
        if declared.is_empty() {
            log::warn!(base_url = self.base.as_str(); "provider declares no models");
        }
        let registry = declared.into_iter().map(|m| (m.name.clone(), m)).collect();
        *self.models.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(registry);
    }

    fn registry(&self) -> Arc<BTreeMap<ModelId, ModelDecl>> {
        Arc::clone(&self.models.read().unwrap_or_else(PoisonError::into_inner))
    }

    fn model(&self, name: &ModelId) -> Result<ModelDecl, ProviderError> {
        self.registry()
            .get(name)
            .cloned()
            .ok_or_else(|| ProviderError {
                kind: ProviderErrorKind::NotFound,
                http_status: None,
                raw: format!("model {name} is not declared by this provider"),
                retry_after: None,
            })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base.as_str().trim_end_matches('/'))
    }

    fn authorized(&self, request: RequestBuilder) -> RequestBuilder {
        let mut request = request;
        if let Some(key) = &self.config.api_key {
            request = request.bearer_auth(key.expose());
        }
        if let Some(referer) = &self.config.referer {
            request = request.header("HTTP-Referer", referer.as_str());
        }
        if let Some(title) = &self.config.title {
            request = request.header("X-Title", title.as_str());
        }
        request
    }

    async fn send(&self, request: RequestBuilder) -> Result<reqwest::Response, ProviderError> {
        let response = self
            .authorized(request)
            .send()
            .await
            .map_err(|e| error::transport(&e))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let retry_after = error::retry_after(response.headers());
        let body = response.text().await.unwrap_or_default();
        Err(classify(status.as_u16(), retry_after, &body))
    }

    async fn fetch_listing(&self) -> Result<Value, ProviderError> {
        let response = self.send(self.client.get(self.url("models"))).await?;
        response
            .json()
            .await
            .map_err(|e| error::other(format!("models listing: {e}")))
    }
}

impl Provider for OpenAiProvider {
    fn decl(&self) -> ProviderDecl {
        ProviderDecl {
            local: is_loopback(&self.base),
            count_tokens: false,
        }
    }

    fn models(&self) -> Vec<ModelDecl> {
        self.registry().values().cloned().collect()
    }

    async fn generate(&self, req: GenerateRequest, mut sink: GenSink) -> Result<(), ProviderError> {
        let model = self.model(&req.model)?;
        let body = render(&req, &model, self.config.dialect)?;
        log::debug!(model = req.model.as_str(), messages = req.messages.len(); "chat completion requested");

        let response = self
            .send(self.client.post(self.url("chat/completions")).json(&body))
            .await?;
        let decoder = Decoder::new(req.model, req.cache)
            .with_rate_limits(error::rate_limits(response.headers()));
        pump(response.bytes_stream(), decoder, &mut sink).await
    }

    async fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse, ProviderError> {
        if self.model(&req.model)?.embed.is_none() {
            return Err(error::invalid(format!(
                "model {} is not an embedding model",
                req.model
            )));
        }
        if req.inputs.is_empty() {
            return Ok(EmbedResponse {
                vectors: Vec::new(),
                usage: Usage::default(),
            });
        }
        let body = json!({"model": req.model.as_str(), "input": req.inputs});
        let response = self
            .send(self.client.post(self.url("embeddings")).json(&body))
            .await?;
        let body: Value = response
            .json()
            .await
            .map_err(|e| error::other(format!("embeddings: {e}")))?;
        embeddings(&body, req.inputs.len())
    }

    async fn count_tokens(&self, _req: GenerateRequest) -> Result<u32, ProviderError> {
        Err(error::invalid(
            "count_tokens is not supported: ProviderDecl.count_tokens is false",
        ))
    }
}

/// Порядок по `index`: сервер не обязан держать порядок входов.
fn embeddings(body: &Value, expected: usize) -> Result<EmbedResponse, ProviderError> {
    let malformed = || error::other(format!("malformed embeddings response: {body}"));
    let mut items = body["data"]
        .as_array()
        .ok_or_else(malformed)?
        .iter()
        .enumerate()
        .map(|(position, item)| {
            let index = item["index"]
                .as_u64()
                .and_then(|i| usize::try_from(i).ok())
                .unwrap_or(position);
            let vector = item["embedding"]
                .as_array()
                .ok_or_else(malformed)?
                .iter()
                .map(|x| x.as_f64().map(|x| x as f32).ok_or_else(malformed))
                .collect::<Result<Vec<f32>, _>>()?;
            Ok((index, vector))
        })
        .collect::<Result<Vec<_>, ProviderError>>()?;
    if items.len() != expected {
        return Err(malformed());
    }
    items.sort_by_key(|(index, _)| *index);
    let number = |key: &str| body["usage"][key].as_u64().unwrap_or(0);
    Ok(EmbedResponse {
        vectors: items.into_iter().map(|(_, v)| v).collect(),
        usage: Usage {
            input: number("prompt_tokens"),
            cost: body["usage"]["cost"].as_f64(),
            ..Usage::default()
        },
    })
}

/// Инференс на машине ядра: только такому эмбеддеру ядро отдаёт private-текст.
fn is_loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}
