//! Эмбеддер за трейтом `Provider`: локальный OpenAI-совместимый сервер
//! `/v1/embeddings` (llama-server, text-embeddings-inference, vLLM, …).
//! Веса модели живут в сервере, не в soul: крейт — только HTTP-клиент.
//!
//! Процессный провайдер криптографии rustls ставит бинарь до
//! [`EmbedLocalProvider::new`]: без него сборка HTTP-клиента паникует.

use std::{collections::BTreeMap, net::IpAddr, time::Duration};

use eva::{data, time::HumanDuration, utils::SecretString};
use reqwest::Url;
use serde_json::{Value, json};

use eva_core::{GenSink, Provider};
use eva_protocol::types::{
    ModelId,
    generate::{
        EmbedRequest, EmbedResponse, GenerateRequest, ProviderError, ProviderErrorKind, Usage,
    },
    model::{EmbedDecl, ModelCaps, ModelDecl, ProviderDecl},
};

pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8080/v1";

/// `llm.providers.<name>` с `kind: embed_local`.
#[data]
#[serde(default)]
pub struct Config {
    /// Корень API без `/embeddings`.
    pub base_url: String,
    /// `{plaintext: …}` или `{file: …}`; локальным серверам обычно не нужен.
    pub api_key: Option<SecretString>,
    /// Потолок подключения и ответа одного запроса.
    pub timeout: HumanDuration,
    /// Модели сервера: `GET /models` размерности не отдаёт, поэтому они объявляются здесь.
    pub models: BTreeMap<ModelId, EmbedDecl>,
    /// `None` — по хосту `base_url` (loopback); `Some(true)` — хозяин ручается, что
    /// сервер его: только такому эмбеддеру soul отдаёт private-текст.
    pub local: Option<bool>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            api_key: None,
            timeout: HumanDuration(Duration::from_secs(30)),
            models: BTreeMap::new(),
            local: None,
        }
    }
}

/// Провайдер не собрался: конфиг.
#[data(error, display("{reason}"))]
pub struct BuildError {
    pub reason: String,
}

pub struct EmbedLocalProvider {
    config: Config,
    api_key: Option<String>,
    base: Url,
    client: reqwest::Client,
}

impl EmbedLocalProvider {
    /// Сеть при сборке не трогается: сервер может подняться позже soul.
    pub fn new(config: Config) -> Result<Self, BuildError> {
        let fail = |reason: String| BuildError { reason };
        let base = Url::parse(config.base_url.trim_end_matches('/'))
            .map_err(|e| fail(format!("base_url: {e}")))?;
        let api_key = match &config.api_key {
            Some(secret) => Some(
                secret
                    .read()
                    .map_err(|e| fail(format!("api_key: {e:#}")))?
                    .trim_end()
                    .to_owned(),
            ),
            None => None,
        };
        if config.models.is_empty() {
            log::warn!(base_url = base.as_str(); "embed_local provider declares no models");
        }
        let client = reqwest::Client::builder()
            .connect_timeout(*config.timeout)
            .timeout(*config.timeout)
            .build()
            .map_err(|e| fail(format!("http client: {e}")))?;
        Ok(Self {
            config,
            api_key,
            base,
            client,
        })
    }

    fn model(&self, name: &ModelId) -> Result<EmbedDecl, ProviderError> {
        self.config.models.get(name).copied().ok_or_else(|| {
            error(
                ProviderErrorKind::NotFound,
                format!("model {name} is not declared by this provider"),
            )
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base.as_str().trim_end_matches('/'))
    }
}

impl Provider for EmbedLocalProvider {
    fn decl(&self) -> ProviderDecl {
        ProviderDecl {
            local: self.config.local.unwrap_or_else(|| is_loopback(&self.base)),
            count_tokens: false,
        }
    }

    fn models(&self) -> Vec<ModelDecl> {
        self.config
            .models
            .iter()
            .map(|(name, embed)| ModelDecl {
                name: name.clone(),
                context_window: embed.max_input_tokens,
                max_output: 0,
                caps: ModelCaps::default(),
                price: None,
                embed: Some(*embed),
            })
            .collect()
    }

    async fn generate(&self, req: GenerateRequest, _sink: GenSink) -> Result<(), ProviderError> {
        Err(error(
            ProviderErrorKind::Invalid,
            format!("model {} only embeds; it cannot generate", req.model),
        ))
    }

    async fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse, ProviderError> {
        let decl = self.model(&req.model)?;
        if req.inputs.is_empty() {
            return Ok(EmbedResponse {
                vectors: Vec::new(),
                usage: Usage::default(),
            });
        }
        let mut request = self
            .client
            .post(self.url("embeddings"))
            .json(&json!({"model": req.model.as_str(), "input": req.inputs}));
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await.map_err(transport)?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError {
                kind: kind_of(status.as_u16()),
                http_status: Some(status.as_u16()),
                raw: body,
                retry_after: None,
            });
        }
        let body: Value = response
            .json()
            .await
            .map_err(|e| error(ProviderErrorKind::Other, format!("embeddings: {e}")))?;
        let response = embeddings(&body, req.inputs.len())?;
        if let Some(bad) = response
            .vectors
            .iter()
            .find(|v| v.len() != decl.dims as usize)
        {
            return Err(error(
                ProviderErrorKind::Invalid,
                format!(
                    "model {} returned {} dims, config declares {}",
                    req.model,
                    bad.len(),
                    decl.dims
                ),
            ));
        }
        Ok(response)
    }

    async fn count_tokens(&self, _req: GenerateRequest) -> Result<u32, ProviderError> {
        Err(error(
            ProviderErrorKind::Invalid,
            "count_tokens is not supported: ProviderDecl.count_tokens is false",
        ))
    }
}

fn kind_of(status: u16) -> ProviderErrorKind {
    match status {
        408 | 429 | 500..=599 => ProviderErrorKind::Transient,
        401 | 403 => ProviderErrorKind::Auth,
        404 => ProviderErrorKind::NotFound,
        400..=499 => ProviderErrorKind::Invalid,
        _ => ProviderErrorKind::Other,
    }
}

fn transport(e: reqwest::Error) -> ProviderError {
    let kind = if e.is_builder() || e.is_request() && !e.is_connect() && !e.is_timeout() {
        ProviderErrorKind::Invalid
    } else {
        ProviderErrorKind::Transient
    };
    error(kind, e.to_string())
}

/// Порядок по `index`: сервер не обязан держать порядок входов.
fn embeddings(body: &Value, expected: usize) -> Result<EmbedResponse, ProviderError> {
    let malformed = || {
        error(
            ProviderErrorKind::Other,
            format!("malformed embeddings response: {body}"),
        )
    };
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
    Ok(EmbedResponse {
        vectors: items.into_iter().map(|(_, v)| v).collect(),
        usage: Usage {
            input: body["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            ..Usage::default()
        },
    })
}

fn error(kind: ProviderErrorKind, raw: impl Into<String>) -> ProviderError {
    ProviderError {
        kind,
        http_status: None,
        raw: raw.into(),
        retry_after: None,
    }
}

/// Инференс на машине soul: только такому эмбеддеру soul отдаёт private-текст.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn build(config: Config) -> EmbedLocalProvider {
        _ = rustls::crypto::ring::default_provider().install_default();
        EmbedLocalProvider::new(config).expect("builds")
    }

    fn config(base_url: &str, local: Option<bool>) -> Config {
        Config {
            base_url: base_url.into(),
            local,
            ..Config::default()
        }
    }

    #[test]
    fn local_follows_the_host_unless_asserted() {
        for (url, want) in [
            ("http://127.0.0.1:8080/v1", true),
            ("http://localhost:8080", true),
            ("http://[::1]:8080/v1", true),
            ("http://192.168.1.10:8080/v1", false),
            ("https://embed.example.org/v1", false),
        ] {
            let p = build(config(url, None));
            assert_eq!(p.decl().local, want, "{url}");
        }
        let p = build(config("http://192.168.1.10:8080/v1", Some(true)));
        assert!(p.decl().local, "the owner vouched for the host");
    }

    #[test]
    fn models_come_from_config_only() {
        let mut cfg = config(DEFAULT_BASE_URL, None);
        cfg.models.insert(
            ModelId::from("small"),
            EmbedDecl {
                dims: 384,
                max_input_tokens: 512,
            },
        );
        let p = build(cfg);
        let models = p.models();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].embed.map(|e| e.dims), Some(384));
        assert_eq!(models[0].context_window, 512);
        assert_eq!(models[0].max_output, 0);
    }

    #[test]
    fn response_is_reordered_by_index() {
        let body = json!({"data": [
            {"index": 1, "embedding": [0.0, 1.0]},
            {"index": 0, "embedding": [1.0, 0.0]},
        ], "usage": {"prompt_tokens": 7}});
        let res = embeddings(&body, 2).expect("parses");
        assert_eq!(res.vectors, [vec![1.0, 0.0], vec![0.0, 1.0]]);
        assert_eq!(res.usage.input, 7);
        assert!(embeddings(&body, 3).is_err(), "count mismatch is malformed");
        assert!(embeddings(&json!({"data": "x"}), 1).is_err());
    }

    #[test]
    fn bad_base_url_is_a_build_error() {
        assert!(EmbedLocalProvider::new(config("not a url", None)).is_err());
    }
}
