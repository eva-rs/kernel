//! HTTP-мост JSON-RPC над настоящим ядром (`06 §1`): слушатель `{http: addr}`
//! компонента `modules`, токены клиентов как у `hello`, `Policy` библиотеки.

mod common;

use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use common::{CLIENT, Harness, token, with_modules};

use eva_kernel::{
    config::ModulesConfig,
    subsystems::modules::{Listening, auth::REFUSED_DELAY},
};
use eva_protocol::types::ModuleId;
use eva_rpc::config::{Cors, HttpListen, Listen};
use reqwest::{
    StatusCode,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE},
};
use serde_json::{Value, json};

const JSON: &str = "application/json";
const MSGPACK: &str = "application/msgpack";

/// Порт, свободный на момент пробы: слушатель поднимается следом.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("probe port")
        .local_addr()
        .expect("probe address")
        .port()
}

struct Rig {
    h: Harness,
    url: String,
    client: reqwest::Client,
    _listening: Listening<common::Test>,
}

async fn rig() -> Rig {
    // Клиент на rustls без провайдера по умолчанию: ставим его, как бинарь ядра.
    eva_rpc::install_crypto_provider();
    let port = free_port();
    let h = common::start_with(|cfg| {
        with_modules(cfg);
        cfg.modules.listen.push(Listen::Http(HttpListen {
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            cors: Cors::default(),
        }));
    })
    .await;
    let (listening, _) = h.listen().await;
    Rig {
        h,
        url: format!("http://127.0.0.1:{port}/v1/rpc"),
        client: reqwest::Client::new(),
        _listening: listening,
    }
}

fn request(id: Value, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

impl Rig {
    async fn send(
        &self,
        token: Option<&str>,
        content_type: &str,
        accept: Option<&str>,
        body: Vec<u8>,
    ) -> reqwest::Response {
        let mut request = self
            .client
            .post(&self.url)
            .header(CONTENT_TYPE, content_type)
            .body(body);
        if let Some(token) = token {
            request = request.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(accept) = accept {
            request = request.header(ACCEPT, accept);
        }
        request.send().await.expect("http request")
    }

    /// JSON-запрос клиента `CLIENT`: ответ `200` разобран.
    async fn json(&self, body: Value) -> Value {
        let response = self
            .send(
                Some(&token(CLIENT)),
                JSON,
                None,
                serde_json::to_vec(&body).expect("encode"),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.expect("json body")
    }

    /// msgpack-запрос клиента `CLIENT`: ответ `200` в msgpack, разобран в `Value`.
    async fn msgpack(&self, body: Value) -> Value {
        let response = self
            .send(
                Some(&token(CLIENT)),
                MSGPACK,
                None,
                rmp_serde::to_vec_named(&body).expect("encode"),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(MSGPACK)
        );
        rmp_serde::from_slice(&response.bytes().await.expect("body")).expect("msgpack body")
    }

    async fn status_of(&self, token: Option<&str>) -> StatusCode {
        let body =
            serde_json::to_vec(&request(json!(1), "surfaces.list", json!({}))).expect("encode");
        self.send(token, JSON, None, body).await.status()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn json_and_msgpack_roundtrips_through_the_kernel() {
    let rig = rig().await;

    let local = rig
        .json(request(
            json!(1),
            "surfaces.get",
            json!({"surface": "local"}),
        ))
        .await;
    assert_eq!(local["id"], 1);
    assert_eq!(local["result"]["surface"]["id"], "local", "{local}");

    let created = rig
        .json(request(
            json!("f1"),
            "folders.create",
            json!({"surface": "local", "name": "http"}),
        ))
        .await;
    assert_eq!(created["id"], "f1");
    assert_eq!(created["result"]["folder"]["name"], "http", "{created}");

    let listed = rig
        .msgpack(request(
            json!(2),
            "folders.list",
            json!({"surface": "local"}),
        ))
        .await;
    let names: Vec<&str> = listed["result"]["found"]
        .as_array()
        .expect("folders")
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert!(names.contains(&"http"), "{names:?}");

    let created = rig
        .msgpack(request(
            json!(3),
            "folders.create",
            json!({"surface": "local", "name": "mp"}),
        ))
        .await;
    assert_eq!(created["result"]["folder"]["name"], "mp", "{created}");

    // Своя ошибка операции разбирается как `Generic<Err>` в `data.error`.
    let twice = rig
        .json(request(
            json!(4),
            "folders.create",
            json!({"surface": "local", "name": "mp"}),
        ))
        .await;
    assert_eq!(twice["error"]["code"], -32009, "{twice}");
    assert_eq!(twice["error"]["data"]["code"], "conflict");
    assert_eq!(
        twice["error"]["data"]["error"]["name_taken"]["name"], "mp",
        "{twice}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bearer_tokens_follow_the_hello_rules() {
    let rig = rig().await;

    assert_eq!(rig.status_of(None).await, StatusCode::UNAUTHORIZED);
    let started = Instant::now();
    assert_eq!(
        rig.status_of(Some("not-a-token")).await,
        StatusCode::UNAUTHORIZED
    );
    assert!(
        started.elapsed() >= REFUSED_DELAY - Duration::from_millis(20),
        "a refused token waits REFUSED_DELAY"
    );

    // Выданный токен работает как токен конфига, отозванный — нет.
    let issued = rig
        .h
        .client_session()
        .await
        .modules()
        .token_issue(ModuleId::from("tablet"), None)
        .await
        .expect("token_issue")
        .token;
    assert_eq!(rig.status_of(Some(&issued)).await, StatusCode::OK);
    rig.h
        .client_session()
        .await
        .modules()
        .token_revoke(ModuleId::from("tablet"))
        .await
        .expect("token_revoke");
    assert_eq!(rig.status_of(Some(&issued)).await, StatusCode::UNAUTHORIZED);

    // Одноразовые сессии не регистрируют модуль: строки `modules` нет.
    let rows: i64 = rig
        .h
        .query(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM modules WHERE name = 'tablet'",
                [],
                |r| r.get(0),
            )?)
        })
        .await;
    assert_eq!(rows, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_batches_and_ws_only_operations() {
    let rig = rig().await;

    let batch = rig
        .json(json!([
            request(json!(1), "surfaces.get", json!({"surface": "local"})),
            request(
                json!(2),
                "surfaces.create",
                json!({"id": "local", "name": "Local", "rules": null})
            ),
            request(json!(3), "turns.attach", json!({"chat": 1})),
            request(
                json!(4),
                "blobs.put",
                json!({"chat": 1, "name": "f", "size": 0})
            ),
        ]))
        .await;
    let batch = batch.as_array().expect("batch");
    assert_eq!(batch.len(), 4);
    assert_eq!(batch[0]["result"]["surface"]["id"], "local");

    // `Policy` библиотеки: builtin surface не создать никому.
    assert_eq!(batch[1]["id"], 2);
    assert_eq!(batch[1]["error"]["code"], -32003, "{}", batch[1]);
    assert_eq!(batch[1]["error"]["data"]["code"], "forbidden");

    for refused in &batch[2..] {
        assert_eq!(refused["error"]["code"], -32601, "{refused}");
        assert_eq!(refused["error"]["message"], "use ws");
    }
}

#[test]
fn listen_config_takes_http_and_https_by_address_key() {
    let modules: ModulesConfig = serde_json::from_value(json!({
        "listen": [
            {"http": "127.0.0.1:7080"},
            {"https": "0.0.0.0:7443", "tls": {"cert": "/etc/eva/cert.pem", "key": "/etc/eva/key.pem"}, "cors": ["https://app.example"]},
        ],
        "max_batch": 8,
    }))
    .expect("modules");
    let [Listen::Http(http), Listen::Https(https)] = modules.listen.as_slice() else {
        panic!("wrong variants: {:?}", modules.listen);
    };
    assert_eq!(http.addr.port(), 7080);
    assert!(http.cors.0.is_empty());
    assert_eq!(https.addr.port(), 7443);
    assert_eq!(https.tls.key.to_str(), Some("/etc/eva/key.pem"));
    assert_eq!(https.cors.0, ["https://app.example"]);
    assert_eq!(modules.transport.max_batch(), 8);

    let defaults: ModulesConfig = serde_json::from_value(json!({})).expect("defaults");
    assert_eq!(defaults.transport.max_batch(), 64);
    assert!(serde_json::from_value::<Listen>(json!({"https": "0.0.0.0:7443"})).is_err());
}
