//! `ProviderConfig` под тегом `kind` и диспетчер `AnyProvider`.
//!
//! У крейта нет dev-зависимостей: JSON и рантайма нет — конфиг читается
//! десериализатором карты serde, а future, не дошедшие до I/O, — одним poll.

use std::{
    future::Future,
    pin::pin,
    task::{Context, Poll, Waker},
};

use serde::{
    Deserialize,
    de::value::{Error, MapDeserializer},
};

use eva::utils::SecretString;
use eva_core::Provider;
use eva_protocol::types::{ModelId, generate::EmbedRequest};
use eva_providers::{AnyProvider, ProviderConfig, build, mock, openai};

fn parse(pairs: &[(&'static str, &'static str)]) -> Result<ProviderConfig, Error> {
    ProviderConfig::deserialize(MapDeserializer::<_, Error>::new(pairs.iter().copied()))
}

fn ready<F: Future>(future: F) -> F::Output {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("future is expected to be ready without I/O"),
    }
}

#[test]
fn openai_kind() {
    let config = parse(&[
        ("kind", "openai"),
        ("base_url", "http://127.0.0.1:8080/v1"),
        ("dialect", "plain"),
        ("timeout", "30s"),
        ("title", "Eva"),
    ])
    .unwrap();
    let ProviderConfig::Openai(config) = config else {
        panic!("expected openai, got {config:?}");
    };
    assert_eq!(config.base_url, "http://127.0.0.1:8080/v1");
    assert_eq!(config.dialect, openai::Dialect::Plain);
    assert_eq!(config.timeout.as_secs(), 30);
    // форма секрета — вложенная (`{plaintext}`/`{file}`), её разбор проверяют тесты крейта openai
    assert!(config.api_key.is_none());
    assert_eq!(config.title.as_deref(), Some("Eva"));
    assert!(config.models.is_empty() && config.allow.is_none());

    let ProviderConfig::Openai(defaults) = parse(&[("kind", "openai")]).unwrap() else {
        panic!("expected openai");
    };
    assert_eq!(defaults.base_url, openai::OPENROUTER_BASE_URL);
    assert_eq!(defaults.dialect, openai::Dialect::Openrouter);
}

#[test]
fn mock_kind_builds_and_dispatches() {
    let config = parse(&[("kind", "mock"), ("on_exhausted", "fail")]).unwrap();
    let ProviderConfig::Mock(ref inner) = config else {
        panic!("expected mock, got {config:?}");
    };
    assert_eq!(inner.on_exhausted, mock::Exhausted::Fail);

    let provider = ready(build("test", config)).unwrap();
    assert!(matches!(provider, AnyProvider::Mock(_)));
    assert!(provider.decl().local);
    let names: Vec<_> = provider.models().into_iter().map(|m| m.name.0).collect();
    assert_eq!(names, [mock::CHAT_MODEL, mock::EMBED_MODEL]);

    let embedded = ready(provider.embed(EmbedRequest {
        model: ModelId::from(mock::EMBED_MODEL),
        inputs: vec!["hi".into()],
    }))
    .unwrap();
    assert_eq!(embedded.vectors[0].len(), 64);
}

#[test]
fn unknown_kind_is_rejected() {
    let error = parse(&[("kind", "acme")]).unwrap_err().to_string();
    assert!(error.contains("acme"), "{error}");
    assert!(parse(&[("base_url", "x")]).is_err(), "kind is required");
}

/// Ошибка сборки — отказ старта soul: она называет путь ключа конфига.
#[test]
fn build_error_names_the_config_key_path() {
    // ключ читается до сборки HTTP-клиента: ни рантайма, ни провайдера rustls тесту не нужно
    let config = ProviderConfig::Openai(openai::Config {
        api_key: Some(SecretString::File("/nonexistent/eva-test-key".into())),
        ..Default::default()
    });
    let Err(error) = ready(build("router", config)) else {
        panic!("unreadable api_key must fail the build");
    };
    let error = error.to_string();
    assert!(
        error.starts_with("llm.providers.router: api_key:"),
        "{error}"
    );
}

#[test]
fn embed_local_kind() {
    let config = parse(&[
        ("kind", "embed_local"),
        ("base_url", "http://127.0.0.1:9000/v1"),
        ("timeout", "5s"),
    ])
    .unwrap();
    let ProviderConfig::EmbedLocal(config) = config else {
        panic!("expected embed_local, got {config:?}");
    };
    assert_eq!(config.base_url, "http://127.0.0.1:9000/v1");
    assert_eq!(config.timeout.as_secs(), 5);
    assert!(config.models.is_empty() && config.local.is_none());

    let ProviderConfig::EmbedLocal(defaults) = parse(&[("kind", "embed_local")]).unwrap() else {
        panic!("expected embed_local");
    };
    assert_eq!(
        defaults.base_url,
        eva_providers::embed_local::DEFAULT_BASE_URL
    );
}
