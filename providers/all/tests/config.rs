//! `ProviderConfig` под тегом `kind` и диспетчер `AnyProvider`.
//!
//! У крейта нет dev-зависимостей: JSON и рантайма нет — конфиг читается
//! десериализатором карты serde, а готовые сразу future мока — одним poll.

use std::{
    future::Future,
    pin::pin,
    task::{Context, Poll, Waker},
};

use serde::{
    Deserialize,
    de::value::{Error, MapDeserializer},
};

use eva_core::Provider;
use eva_protocol::types::{ModelId, generate::EmbedRequest};
use eva_providers::{AnyProvider, ProviderConfig, build, mock, openai};

fn parse(pairs: &[(&'static str, &'static str)]) -> Result<ProviderConfig, Error> {
    ProviderConfig::deserialize(MapDeserializer::<_, Error>::new(pairs.iter().copied()))
}

/// Future мока не ждёт ничего: готов с первого poll.
fn ready<F: Future>(future: F) -> F::Output {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("mock future is expected to be ready at once"),
    }
}

#[test]
fn openai_kind() {
    let config = parse(&[
        ("kind", "openai"),
        ("base_url", "http://127.0.0.1:8080/v1"),
        ("dialect", "plain"),
        ("timeout", "30s"),
        ("api_key", "sk-local"),
        ("title", "Eva"),
    ])
    .unwrap();
    let ProviderConfig::Openai(config) = config else {
        panic!("expected openai, got {config:?}");
    };
    assert_eq!(config.base_url, "http://127.0.0.1:8080/v1");
    assert_eq!(config.dialect, openai::Dialect::Plain);
    assert_eq!(config.timeout.as_secs(), 30);
    assert_eq!(
        config.api_key.as_ref().map(openai::ApiKey::expose),
        Some("sk-local")
    );
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
