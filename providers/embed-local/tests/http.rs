//! Путь через настоящий HTTP-клиент против loopback-сервера.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

use eva_core::Provider;
use eva_protocol::types::{
    ModelId,
    generate::{EmbedRequest, ProviderErrorKind},
    model::EmbedDecl,
};
use eva_provider_embed_local::{Config, EmbedLocalProvider};

struct Reply {
    status: u16,
    body: String,
}

/// Один ответ на соединение, по порядку; возвращает тела запросов.
async fn serve(replies: Vec<Reply>) -> (String, JoinHandle<Vec<(String, Value)>>) {
    _ = rustls::crypto::ring::default_provider().install_default();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
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
            let length: usize = head
                .lines()
                .filter_map(|l| l.split_once(':'))
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .map_or(0, |(_, v)| v.trim().parse().unwrap());
            let mut body = raw[split + 4..].to_vec();
            while body.len() < length {
                let n = socket.read(&mut buf).await.unwrap();
                body.extend_from_slice(&buf[..n]);
            }
            seen.push((
                head.lines().next().unwrap().to_owned(),
                serde_json::from_slice(&body).unwrap_or(Value::Null),
            ));
            let response = format!(
                "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                reply.status,
                reply.body.len(),
                reply.body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
        seen
    });
    (base, task)
}

fn provider(base: String) -> EmbedLocalProvider {
    EmbedLocalProvider::new(Config {
        base_url: base,
        models: BTreeMap::from([(
            ModelId::from("tiny"),
            EmbedDecl {
                dims: 3,
                max_input_tokens: 512,
            },
        )]),
        ..Config::default()
    })
    .expect("builds")
}

fn request(inputs: &[&str]) -> EmbedRequest {
    EmbedRequest {
        model: ModelId::from("tiny"),
        inputs: inputs.iter().map(|s| (*s).to_owned()).collect(),
    }
}

#[tokio::test]
async fn embeds_through_the_plain_endpoint() {
    let (base, seen) = serve(vec![Reply {
        status: 200,
        body: json!({"data": [
            {"index": 1, "embedding": [0.0, 0.0, 1.0]},
            {"index": 0, "embedding": [1.0, 0.0, 0.0]},
        ], "usage": {"prompt_tokens": 4}})
        .to_string(),
    }])
    .await;
    let p = provider(base);
    assert!(p.decl().local);

    let res = p.embed(request(&["a", "b"])).await.expect("embeds");
    assert_eq!(res.vectors, [vec![1.0, 0.0, 0.0], vec![0.0, 0.0, 1.0]]);
    assert_eq!(res.usage.input, 4);

    let seen = seen.await.unwrap();
    assert_eq!(seen[0].0, "POST /v1/embeddings HTTP/1.1");
    assert_eq!(seen[0].1["model"], "tiny");
    assert_eq!(seen[0].1["input"], json!(["a", "b"]));
}

#[tokio::test]
async fn wrong_dims_and_server_errors_are_typed() {
    let (base, _seen) = serve(vec![
        Reply {
            status: 200,
            body: json!({"data": [{"index": 0, "embedding": [1.0, 0.0]}]}).to_string(),
        },
        Reply {
            status: 503,
            body: String::from("busy"),
        },
    ])
    .await;
    let p = provider(base);

    let err = p.embed(request(&["a"])).await.expect_err("dims differ");
    assert_eq!(err.kind, ProviderErrorKind::Invalid);
    let err = p.embed(request(&["a"])).await.expect_err("503");
    assert_eq!(err.kind, ProviderErrorKind::Transient);
    assert_eq!(err.http_status, Some(503));
}

#[tokio::test]
async fn unknown_model_and_empty_input_do_not_hit_the_network() {
    _ = rustls::crypto::ring::default_provider().install_default();
    let p = provider(String::from("http://127.0.0.1:9/v1"));
    let err = p
        .embed(EmbedRequest {
            model: ModelId::from("other"),
            inputs: vec![String::from("a")],
        })
        .await
        .expect_err("undeclared model");
    assert_eq!(err.kind, ProviderErrorKind::NotFound);
    let res = p.embed(request(&[])).await.expect("empty is a no-op");
    assert!(res.vectors.is_empty());
}
