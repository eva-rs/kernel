//! Путь через настоящий HTTP-клиент против loopback-сервера с заготовленными ответами.

use std::{collections::BTreeMap, time::Duration};

use futures::{StreamExt, channel::mpsc};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

use eva_core::{GenSink, Provider};
use eva_protocol::types::{
    ModelId,
    generate::{
        Block, CacheMode, EmbedRequest, GenItem, GenerateRequest, Message, ProviderErrorKind,
        Purpose, Stop, StopReason, SystemSection,
    },
};
use eva_provider_openai::{ApiKey, Config, Dialect, ModelOverride, OpenAiProvider};

struct Reply {
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    body: String,
}

impl Reply {
    fn ok(content_type: &'static str, body: impl Into<String>) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type", content_type)],
            body: body.into(),
        }
    }
}

#[derive(Debug)]
struct Seen {
    head: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

/// Один ответ на соединение, по порядку; тело закрывается закрытием сокета.
async fn serve(replies: Vec<Reply>) -> (String, JoinHandle<Vec<Seen>>) {
    _ = rustls::crypto::ring::default_provider().install_default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut seen = Vec::new();
        for reply in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            let mut buf = [0_u8; 4096];
            let split = loop {
                let n = socket.read(&mut buf).await.unwrap();
                raw.extend_from_slice(&buf[..n]);
                if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    break at;
                }
            };
            let head = String::from_utf8(raw[..split].to_vec()).unwrap();
            let mut lines = head.lines();
            let first = lines.next().unwrap().to_owned();
            let headers: BTreeMap<String, String> = lines
                .filter_map(|l| l.split_once(':'))
                .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_owned()))
                .collect();
            let length: usize = headers
                .get("content-length")
                .map_or(0, |v| v.parse().unwrap());
            let mut body = raw[split + 4..].to_vec();
            while body.len() < length {
                let n = socket.read(&mut buf).await.unwrap();
                body.extend_from_slice(&buf[..n]);
            }
            seen.push(Seen {
                head: first,
                headers,
                body: serde_json::from_slice(&body).unwrap_or(Value::Null),
            });

            let mut out = format!("HTTP/1.1 {} X\r\nconnection: close\r\n", reply.status);
            for (k, v) in &reply.headers {
                out.push_str(&format!("{k}: {v}\r\n"));
            }
            out.push_str("\r\n");
            out.push_str(&reply.body);
            socket.write_all(out.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
        seen
    });
    (base, task)
}

fn request(model: &str) -> GenerateRequest {
    GenerateRequest {
        model: ModelId::from(model),
        system: vec![SystemSection {
            name: "persona".into(),
            text: "You are Eva.".into(),
        }],
        tools: Vec::new(),
        messages: vec![Message::User {
            blocks: vec![Block::Text { text: "hi".into() }],
        }],
        breakpoints: Vec::new(),
        cache: CacheMode::Off,
        effort: None,
        thinking: None,
        max_tokens: 256,
        format: None,
        purpose: Purpose::Turn,
    }
}

fn listing() -> String {
    include_str!("fixtures/openrouter_models.json").to_owned()
}

#[tokio::test]
async fn connect_then_stream_a_reply() {
    let sse = [
        json!({"choices": [{"index": 0, "delta": {"content": "Hello"}, "finish_reason": null}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 1, "cost": 0.5}}),
    ]
    .iter()
    .map(|c| format!("data: {c}\n\n"))
    .collect::<String>()
        + "data: [DONE]\n\n";
    let (base, server) = serve(vec![
        Reply::ok("application/json", listing()),
        Reply {
            status: 200,
            headers: vec![
                ("content-type", "text/event-stream"),
                ("x-ratelimit-remaining-requests", "41"),
            ],
            body: sse,
        },
    ])
    .await;

    let provider = OpenAiProvider::connect(Config {
        base_url: base,
        api_key: Some(ApiKey::new("sk-test")),
        referer: Some("https://eva.invalid".into()),
        title: Some("Eva".into()),
        ..Config::default()
    })
    .await
    .unwrap();
    assert_eq!(provider.models().len(), 7);
    assert!(provider.decl().local, "loopback base_url");

    let (tx, rx) = mpsc::unbounded();
    provider
        .generate(request("openai/gpt-5-mini"), GenSink::new(tx))
        .await
        .unwrap();
    let items: Vec<GenItem> = rx.collect().await;
    assert_eq!(
        items[0],
        GenItem::TextDelta {
            text: "Hello".into()
        }
    );
    let GenItem::Usage(usage) = &items[1] else {
        panic!("{items:?}");
    };
    assert_eq!((usage.input, usage.output, usage.cost), (10, 1, Some(0.5)));
    assert_eq!(usage.rate_limits.unwrap().requests_remaining, Some(41));
    assert_eq!(
        items[2],
        GenItem::Stop(Stop {
            reason: StopReason::EndTurn,
            details: None,
        })
    );

    let seen = server.await.unwrap();
    assert_eq!(seen[0].head, "GET /api/v1/models HTTP/1.1");
    assert_eq!(seen[1].head, "POST /api/v1/chat/completions HTTP/1.1");
    for request in &seen {
        assert_eq!(request.headers["authorization"], "Bearer sk-test");
        assert_eq!(request.headers["http-referer"], "https://eva.invalid");
        assert_eq!(request.headers["x-title"], "Eva");
    }
    assert_eq!(seen[1].body["model"], "openai/gpt-5-mini");
    assert_eq!(seen[1].body["stream"], true);
}

#[tokio::test]
async fn http_errors_are_classified() {
    let (base, server) = serve(vec![
        Reply::ok("application/json", listing()),
        Reply {
            status: 429,
            headers: vec![("retry-after", "3"), ("content-type", "application/json")],
            body: json!({"error": {"code": 429, "message": "Rate limit exceeded"}}).to_string(),
        },
    ])
    .await;
    let provider = OpenAiProvider::connect(Config {
        base_url: base,
        ..Config::default()
    })
    .await
    .unwrap();
    let (tx, _rx) = mpsc::unbounded();
    let error = provider
        .generate(request("openai/gpt-5-mini"), GenSink::new(tx))
        .await
        .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Transient);
    assert_eq!(error.http_status, Some(429));
    assert_eq!(error.retry_after.map(|d| *d), Some(Duration::from_secs(3)));
    assert!(error.raw.contains("Rate limit exceeded"));
    server.await.unwrap();

    let (tx, _rx) = mpsc::unbounded();
    let unknown = provider
        .generate(request("nope/model"), GenSink::new(tx))
        .await
        .unwrap_err();
    assert_eq!(unknown.kind, ProviderErrorKind::NotFound);
}

#[tokio::test]
async fn openrouter_needs_the_catalog() {
    let (base, server) = serve(vec![Reply {
        status: 503,
        headers: Vec::new(),
        body: "down".into(),
    }])
    .await;
    let error = OpenAiProvider::connect(Config {
        base_url: base,
        ..Config::default()
    })
    .await
    .err()
    .unwrap();
    assert!(error.reason.contains("GET /models"), "{}", error.reason);
    server.await.unwrap();
}

#[tokio::test]
async fn plain_server_without_catalog_embeds() {
    let (base, server) = serve(vec![
        Reply {
            status: 404,
            headers: Vec::new(),
            body: "no such route".into(),
        },
        Reply::ok(
            "application/json",
            json!({
                "data": [
                    {"index": 1, "embedding": [0.0, 1.0]},
                    {"index": 0, "embedding": [1.0, 0.0]}
                ],
                "usage": {"prompt_tokens": 4}
            })
            .to_string(),
        ),
    ])
    .await;
    let config = Config {
        base_url: base,
        dialect: Dialect::Plain,
        models: BTreeMap::from([(
            ModelId::from("bge-m3"),
            ModelOverride {
                embed: Some(eva_protocol::types::model::EmbedDecl {
                    dims: 2,
                    max_input_tokens: 512,
                }),
                ..ModelOverride::default()
            },
        )]),
        ..Config::default()
    };
    let provider = OpenAiProvider::connect(config).await.unwrap();
    assert_eq!(provider.models().len(), 1);

    let response = provider
        .embed(EmbedRequest {
            model: ModelId::from("bge-m3"),
            inputs: vec!["a".into(), "b".into()],
        })
        .await
        .unwrap();
    assert_eq!(response.vectors, [vec![1.0, 0.0], vec![0.0, 1.0]]);
    assert_eq!(response.usage.input, 4);

    let seen = server.await.unwrap();
    assert_eq!(seen[1].head, "POST /api/v1/embeddings HTTP/1.1");
    assert_eq!(
        seen[1].body,
        json!({"model": "bge-m3", "input": ["a", "b"]})
    );
    assert!(
        !seen[1].headers.contains_key("authorization"),
        "no key, no header"
    );

    let count = provider.count_tokens(request("bge-m3")).await.unwrap_err();
    assert_eq!(count.kind, ProviderErrorKind::Invalid);
}
