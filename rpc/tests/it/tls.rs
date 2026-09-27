//! wss: TLS-слушатель из PEM и клиент со своим CA.
//!
//! Сертификаты — `tests/fixtures`: CA и лист для `localhost`/`127.0.0.1`
//! сроком на сто лет.

use std::{path::PathBuf, time::Duration};

use eva::url::Url;
use eva_core::{CallStep as _, service::surfaces::Surfaces as _};
use eva_protocol::requests::surfaces::create;
use eva_rpc::{
    ClientOptions, ClientTls, ConnectError, Listener, NoHandler, RpcClient, Server,
    config::{Listen, TlsFiles, WssListen},
};
use tokio::sync::mpsc;

use crate::{
    harness::{config, hello},
    kernel::{Auth, CLIENT_TOKEN, Kernel},
};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[tokio::test(flavor = "multi_thread")]
async fn wss_with_extra_ca() {
    let cfg = config();
    let listen = Listen::Wss(WssListen {
        addr: "127.0.0.1:0".parse().unwrap(),
        tls: TlsFiles {
            cert: fixture("localhost.pem"),
            key: fixture("localhost.key"),
        },
    });
    let listener = Listener::bind(&listen, &cfg).await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = Server::new(Kernel::default(), Auth::default(), cfg.clone());
    let (sessions_tx, mut sessions) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        server
            .serve(listener, std::future::pending(), move |accepted| {
                _ = sessions_tx.send(accepted);
            })
            .await;
    });

    let target = Url::parse(&format!("wss://127.0.0.1:{}", addr.port())).unwrap();
    let options = ClientOptions {
        config: cfg.clone(),
        tls: ClientTls {
            extra_ca: Some(fixture("ca.pem")),
        },
        ..ClientOptions::default()
    };
    let (client, welcome) =
        RpcClient::connect_with(&target, hello(CLIENT_TOKEN), options, NoHandler)
            .await
            .unwrap();
    let accepted = tokio::time::timeout(Duration::from_secs(5), sessions.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(accepted.welcome.session_id, welcome.session_id);

    let create::Ok { surface } = client
        .session()
        .surfaces()
        .create()
        .call(create::Args {
            id: "tls".into(),
            name: "tls".to_owned(),
            rules: None,
        })
        .await
        .unwrap();
    assert_eq!(surface.id.as_str(), "tls");

    // Самоподписанное ядро не проходит публичные корни webpki.
    let public = ClientOptions {
        config: cfg.clone(),
        ..ClientOptions::default()
    };
    let err = RpcClient::connect_with(&target, hello(CLIENT_TOKEN), public, NoHandler)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ConnectError::Transport(m) if m.contains("UnknownIssuer")),
        "{err:?}"
    );

    // Без CA сертификат ядра не проверяется.
    let untrusted = ClientOptions {
        config: cfg,
        tls: ClientTls {
            extra_ca: Some(fixture("localhost.pem")),
        },
        ..ClientOptions::default()
    };
    let err = RpcClient::connect_with(&target, hello(CLIENT_TOKEN), untrusted, NoHandler)
        .await
        .unwrap_err();
    assert!(matches!(err, ConnectError::Transport(_)), "{err:?}");
}
