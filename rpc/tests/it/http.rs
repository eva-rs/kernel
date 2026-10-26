//! HTTP-мост JSON-RPC (`06 §1`): тот же диспетчер и `Policy`, что у кадров,
//! в конверте JSON или msgpack на `POST /v1/rpc` с `Authorization: Bearer`.

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use eva::bytesize::ByteSize;
use eva_rpc::{
    Config, Listener, Server,
    config::{BatchLimit, Cors, HttpListen, HttpsListen, Listen, TlsFiles},
};
use reqwest::{
    StatusCode,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue, ORIGIN},
};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::{
    harness::config,
    soul::{Auth, CLIENT_TOKEN, Soul},
};

const JSON: &str = "application/json";
const MSGPACK: &str = "application/msgpack";

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn http(cors: Cors) -> Listen {
    Listen::Http(HttpListen {
        addr: "127.0.0.1:0".parse().unwrap(),
        cors,
    })
}

/// Мост над тестовым soul; сервер живёт, пока жив харнесс.
struct Bridge {
    addr: SocketAddr,
    scheme: &'static str,
    soul: Soul,
    client: reqwest::Client,
    _stop: oneshot::Sender<()>,
}

impl Bridge {
    async fn start(listen: Listen, config: Config) -> Self {
        // Клиент на rustls без провайдера по умолчанию: ставим его, как бинарь soul.
        eva_rpc::install_crypto_provider();
        Self::with_client(listen, config, reqwest::Client::new()).await
    }

    async fn with_client(listen: Listen, config: Config, client: reqwest::Client) -> Self {
        let scheme = match listen {
            Listen::Https(_) => "https",
            _ => "http",
        };
        let listener = Listener::bind(&listen, &config).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let soul = Soul::default();
        let server = Server::new(soul.clone(), Auth::default(), config);
        let (stop, stopped) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let shutdown = async {
                _ = stopped.await;
            };
            server.serve(listener, shutdown, |_| {}).await;
        });
        Self {
            addr,
            scheme,
            soul,
            client,
            _stop: stop,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}://{}{path}", self.scheme, self.addr)
    }

    fn post(&self, body: Vec<u8>, content_type: Option<&str>) -> reqwest::RequestBuilder {
        let mut request = self.client.post(self.url("/v1/rpc")).body(body);
        if let Some(content_type) = content_type {
            request = request.header(CONTENT_TYPE, content_type);
        }
        request
    }

    fn authorized(&self, body: &Value) -> reqwest::RequestBuilder {
        self.post(serde_json::to_vec(body).unwrap(), Some(JSON))
            .header(AUTHORIZATION, format!("Bearer {CLIENT_TOKEN}"))
    }

    /// JSON-запрос клиента `CLIENT_TOKEN`: статус и разобранное тело.
    async fn json(&self, body: &Value) -> (StatusCode, Value) {
        let response = self.authorized(body).send().await.unwrap();
        let status = response.status();
        let bytes = response.bytes().await.unwrap();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, body)
    }

    async fn rpc(&self, body: Value) -> Value {
        let (status, body) = self.json(&body).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    async fn msgpack(&self, body: &Value, accept: Option<&str>) -> reqwest::Response {
        let mut request = self
            .post(rmp_serde::to_vec_named(body).unwrap(), Some(MSGPACK))
            .header(AUTHORIZATION, format!("Bearer {CLIENT_TOKEN}"));
        if let Some(accept) = accept {
            request = request.header(ACCEPT, accept);
        }
        request.send().await.unwrap()
    }
}

fn request(id: Value, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn create(id: &str) -> Value {
    json!({"id": id, "name": id.to_uppercase(), "rules": null})
}

fn content_type(response: &reqwest::Response) -> &str {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn json_roundtrip_of_a_mutation_and_a_read() {
    let h = Bridge::start(http(Cors::default()), config()).await;

    let created = h
        .rpc(request(json!(1), "surfaces.create", create("web")))
        .await;
    assert_eq!(created["jsonrpc"], "2.0");
    assert_eq!(created["id"], 1);
    assert_eq!(created["result"]["surface"]["id"], "web");
    assert_eq!(h.soul.state.creates.load(Ordering::SeqCst), 1);

    let got = h
        .rpc(request(
            json!("two"),
            "surfaces.get",
            json!({"surface": "web"}),
        ))
        .await;
    assert_eq!(got["id"], "two");
    assert_eq!(got["result"]["surface"]["name"], "web");

    // Своя ошибка операции: код по `ErrorCode`, `data.error` — `Generic<Err>`.
    let missing = h
        .rpc(request(
            json!(3),
            "surfaces.get",
            json!({"surface": "nope"}),
        ))
        .await;
    assert_eq!(missing["error"]["code"], -32004);
    assert_eq!(missing["error"]["data"]["code"], "not_found");
    assert_eq!(
        missing["error"]["data"]["error"],
        json!({"not_found": {"surface": "nope"}})
    );

    // `Policy` — тот же `Aux::Forbidden`, что на проводе.
    let forbidden = h.rpc(request(json!(4), "surfaces.list", json!({}))).await;
    assert_eq!(forbidden["error"]["code"], -32003);
    assert_eq!(forbidden["error"]["message"], "forbidden: owner only");
    assert_eq!(
        forbidden["error"]["data"],
        json!({"code": "forbidden", "error": {"forbidden": "owner only"}})
    );

    // `params` не `Args` → `invalid_params`; без `params` — `Args` по умолчанию.
    let bad = h
        .rpc(request(json!(5), "surfaces.get", json!({"nope": 1})))
        .await;
    assert_eq!(bad["error"]["code"], -32602);
    assert_eq!(bad["error"]["data"]["code"], "invalid_params");
    let defaulted = h
        .rpc(json!({"jsonrpc": "2.0", "id": 6, "method": "surfaces.list"}))
        .await;
    assert_eq!(defaulted["error"]["code"], -32003, "{defaulted}");
}

#[tokio::test(flavor = "multi_thread")]
async fn msgpack_envelope_mirrors_json_and_accept_picks_the_reply_codec() {
    let h = Bridge::start(http(Cors::default()), config()).await;

    let response = h
        .msgpack(&request(json!(1), "surfaces.create", create("mp")), None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(content_type(&response), MSGPACK);
    let body: Value = rmp_serde::from_slice(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(body["id"], 1);
    assert_eq!(body["result"]["surface"]["id"], "mp");

    // msgpack-запрос, JSON-ответ.
    let response = h
        .msgpack(
            &request(json!(2), "surfaces.get", json!({"surface": "mp"})),
            Some(JSON),
        )
        .await;
    assert_eq!(content_type(&response), JSON);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["result"]["surface"]["id"], "mp");

    // JSON-запрос, msgpack-ответ; `*/*` оставляет кодек запроса.
    let response = h
        .authorized(&request(json!(3), "surfaces.get", json!({"surface": "mp"})))
        .header(ACCEPT, "application/msgpack;q=0.9, */*;q=0.1")
        .send()
        .await
        .unwrap();
    assert_eq!(content_type(&response), MSGPACK);
    let body: Value = rmp_serde::from_slice(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(body["result"]["surface"]["id"], "mp");
    let response = h
        .authorized(&request(json!(4), "surfaces.get", json!({"surface": "mp"})))
        .header(ACCEPT, "*/*")
        .send()
        .await
        .unwrap();
    assert_eq!(content_type(&response), JSON);
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_answers_in_order_and_is_capped() {
    let cfg = Config {
        max_batch: BatchLimit::POS4,
        ..config()
    };
    let h = Bridge::start(http(Cors::default()), cfg).await;

    let batch = h
        .rpc(json!([
            request(json!(1), "surfaces.create", create("a")),
            {"jsonrpc": "2.0", "method": "surfaces.create", "params": create("b")},
            request(json!(3), "surfaces.get", json!({"surface": "b"})),
            7,
        ]))
        .await;
    let batch = batch.as_array().expect("batch answers with an array");
    assert_eq!(batch.len(), 3, "the notification has no answer");
    assert_eq!(batch[0]["id"], 1);
    assert_eq!(batch[0]["result"]["surface"]["id"], "a");
    assert_eq!(batch[1]["id"], 3);
    assert_eq!(batch[1]["result"]["surface"]["id"], "b");
    assert_eq!(batch[2]["id"], Value::Null);
    assert_eq!(batch[2]["error"]["code"], -32600);
    assert_eq!(h.soul.state.creates.load(Ordering::SeqCst), 2);

    let empty = h.rpc(json!([])).await;
    assert_eq!(empty["error"]["code"], -32600);
    let long = h
        .rpc(Value::Array(
            (0..5)
                .map(|n| request(json!(n), "surfaces.list", json!({})))
                .collect(),
        ))
        .await;
    assert_eq!(long["error"]["code"], -32600);
    assert!(
        long["error"]["message"]
            .as_str()
            .unwrap()
            .contains("max_batch"),
        "{long}"
    );

    // Одни уведомления — исполнены, ответа нет.
    let (status, body) = h
        .json(&json!({"jsonrpc": "2.0", "method": "surfaces.create", "params": create("c")}))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(body, Value::Null);
    assert_eq!(h.soul.state.creates.load(Ordering::SeqCst), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_and_uploads_are_ws_only() {
    let h = Bridge::start(http(Cors::default()), config()).await;

    for (method, params) in [
        ("turns.attach", json!({"chat": 1})),
        ("blobs.put", json!({"chat": 1, "name": "f", "size": 0})),
    ] {
        let refused = h.rpc(request(json!(1), method, params)).await;
        assert_eq!(refused["error"]["code"], -32601, "{method}: {refused}");
        assert_eq!(refused["error"]["message"], "use ws");
        assert_eq!(refused["error"]["data"], json!({"code": "unknown_method"}));
    }

    let unknown = h.rpc(request(json!(2), "nope.x", json!({}))).await;
    assert_eq!(unknown["error"]["code"], -32601);
    assert_eq!(unknown["error"]["message"], "unknown method nope.x");
}

#[tokio::test(flavor = "multi_thread")]
async fn bearer_token_is_checked_before_the_body_is_read() {
    let h = Bridge::start(http(Cors::default()), config()).await;
    let body = serde_json::to_vec(&request(json!(1), "surfaces.create", create("x"))).unwrap();

    for authorization in [
        None,
        Some("Basic dXNlcjpwYXNz"),
        Some("Bearer wrong"),
        Some("Bearer"),
    ] {
        let mut request = h.post(body.clone(), Some(JSON));
        if let Some(authorization) = authorization {
            request = request.header(AUTHORIZATION, authorization);
        }
        let response = request.send().await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{authorization:?}"
        );
        assert_eq!(
            response.headers().get("www-authenticate"),
            Some(&HeaderValue::from_static("Bearer"))
        );
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["id"], Value::Null);
        assert_eq!(body["error"]["code"], -32001);
        assert_eq!(body["error"]["data"]["code"], "unauthorized");
    }
    assert_eq!(
        h.soul.state.creates.load(Ordering::SeqCst),
        0,
        "nothing reached the dispatcher"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn media_types_paths_and_body_limit() {
    let cfg = Config {
        max_frame: ByteSize::kib(1),
        ..config()
    };
    let h = Bridge::start(http(Cors::default()), cfg).await;
    let bearer = format!("Bearer {CLIENT_TOKEN}");
    let body = serde_json::to_vec(&request(json!(1), "surfaces.create", create("x"))).unwrap();

    for content_type in [None, Some("text/plain")] {
        let response = h
            .post(body.clone(), content_type)
            .header(AUTHORIZATION, &bearer)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], -32600);
    }

    let huge = serde_json::to_vec(&request(
        json!(1),
        "surfaces.create",
        json!({"id": "x", "name": "n".repeat(2048), "rules": null}),
    ))
    .unwrap();
    let response = h
        .post(huge, Some(JSON))
        .header(AUTHORIZATION, &bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let response = h
        .post(b"{not json".to_vec(), Some(JSON))
        .header(AUTHORIZATION, &bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "parse errors are answered in the envelope"
    );
    let parsed: Value = response.json().await.unwrap();
    assert_eq!(parsed["error"]["code"], -32700);

    let response = h
        .client
        .get(h.url("/v1/rpc"))
        .header(AUTHORIZATION, &bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        response.headers().get("allow"),
        Some(&HeaderValue::from_static("POST, OPTIONS"))
    );
    let response = h
        .client
        .post(h.url("/v1/modules"))
        .header(CONTENT_TYPE, JSON)
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(h.soul.state.creates.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn cors_is_permissive_on_loopback_only_unless_configured() {
    let acao = "access-control-allow-origin";
    let any = Bridge::start(http(Cors::default()), config()).await;
    let response = any
        .authorized(&request(json!(1), "surfaces.list", json!({})))
        .header(ORIGIN, "http://localhost:3000")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(acao),
        Some(&HeaderValue::from_static("http://localhost:3000"))
    );
    assert_eq!(
        response.headers().get("vary"),
        Some(&HeaderValue::from_static("Origin"))
    );

    let preflight = any
        .client
        .request(reqwest::Method::OPTIONS, any.url("/v1/rpc"))
        .header(ORIGIN, "http://localhost:3000")
        .header("access-control-request-method", "POST")
        .send()
        .await
        .unwrap();
    assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        preflight.headers().get("access-control-allow-methods"),
        Some(&HeaderValue::from_static("POST, OPTIONS"))
    );
    assert_eq!(
        preflight.headers().get("access-control-allow-headers"),
        Some(&HeaderValue::from_static(
            "Authorization, Content-Type, Accept"
        ))
    );

    // Без `Origin` — не браузер: заголовков CORS нет.
    let response = any
        .authorized(&request(json!(2), "surfaces.list", json!({})))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get(acao).is_none());

    let listed = Bridge::start(
        http(Cors(vec![String::from("https://app.example")])),
        config(),
    )
    .await;
    let response = listed
        .authorized(&request(json!(1), "surfaces.list", json!({})))
        .header(ORIGIN, "https://app.example")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(acao),
        Some(&HeaderValue::from_static("https://app.example"))
    );
    for method in [reqwest::Method::POST, reqwest::Method::OPTIONS] {
        let response = listed
            .client
            .request(method.clone(), listed.url("/v1/rpc"))
            .header(ORIGIN, "https://evil.example")
            .header(CONTENT_TYPE, JSON)
            .header(AUTHORIZATION, format!("Bearer {CLIENT_TOKEN}"))
            .body(serde_json::to_vec(&request(json!(1), "surfaces.create", create("x"))).unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method}");
        assert!(response.headers().get(acao).is_none());
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["data"]["code"], "forbidden");
    }
    assert_eq!(listed.soul.state.creates.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn https_serves_the_same_bridge() {
    eva_rpc::install_crypto_provider();
    let listen = Listen::Https(HttpsListen {
        addr: "127.0.0.1:0".parse().unwrap(),
        tls: TlsFiles {
            cert: fixture("localhost.pem"),
            key: fixture("localhost.key"),
        },
        cors: Cors::default(),
    });
    let ca = std::fs::read(fixture("ca.pem")).unwrap();
    let client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(&ca).unwrap())
        .build()
        .unwrap();
    let h = Bridge::with_client(listen, config(), client).await;

    let created = h
        .rpc(request(json!(1), "surfaces.create", create("tls")))
        .await;
    assert_eq!(created["result"]["surface"]["id"], "tls");

    // Без TLS на https-слушателе разговора нет.
    let plain = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let started = Instant::now();
    let result = plain
        .post(format!("http://{}/v1/rpc", h.addr))
        .header(CONTENT_TYPE, JSON)
        .body("{}")
        .send()
        .await;
    assert!(result.is_err(), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}
