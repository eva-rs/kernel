//! `run`: обрыв → реконнект с `hello.resume`, продолжением счётчика id и
//! повторной регистрацией surface; `shutdown` закрывает соединение.

use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use eva::url::Url;
use eva_protocol::wire::{ErrorCode, RpcError};
use eva_rpc::{
    Config, Listener,
    config::{FileMode, Listen, UdsListen},
};
use eva_sdk::{Module, Surface};

use crate::fake::{self, Surfaces};

async fn listen(dir: &tempfile::TempDir) -> (Listener, Url) {
    let path = dir.path().join("modules.sock");
    let listener = Listener::bind(
        &Listen::Uds(UdsListen {
            path: path.clone(),
            mode: FileMode::default(),
            group: None,
        }),
        &Config::default(),
    )
    .await
    .unwrap();
    let url = Url::parse(&format!("uds://{}", path.display())).unwrap();
    (listener, url)
}

#[tokio::test]
async fn reconnects_with_resume_and_reregisters_the_surface() {
    let dir = tempfile::tempdir().unwrap();
    let (listener, url) = listen(&dir).await;
    let surfaces = Surfaces::default();
    let (sessions_tx, mut sessions) = mpsc::unbounded_channel();

    let kernel_surfaces = surfaces.clone();
    tokio::spawn(async move {
        for session in ["s1", "s2"] {
            let conn = listener.accept().await.unwrap().upgrade().await.unwrap();
            let accepted =
                fake::accept(conn, fake::welcome(session, &[]), kernel_surfaces.clone()).await;
            if sessions_tx.send(accepted).is_err() {
                return;
            }
        }
        // Держим слушателя, пока тест не закончится.
        std::future::pending::<()>().await;
    });

    let connected = Module::new("x")
        .surface(Surface::new("x").name("X"))
        .reconnect_backoff(Duration::from_millis(10), Duration::from_millis(40))
        .connect(&url, "token")
        .await
        .unwrap();
    let first = sessions.recv().await.unwrap();
    assert!(first.hello.resume.is_none());
    assert_eq!(first.hello.token.expose(), "token");

    let (stop, stopped) = oneshot::channel::<()>();
    let handle = connected.clone();
    let running = tokio::spawn(connected.run(async {
        _ = stopped.await;
    }));

    first
        .peer
        .close(RpcError::new(ErrorCode::Unavailable, "kernel restarts"));
    let second = tokio::time::timeout(Duration::from_secs(5), sessions.recv())
        .await
        .expect("the module reconnects")
        .unwrap();
    let resume = second.hello.resume.as_ref().expect("resume on reconnect");
    assert_eq!(resume.session_id.as_str(), "s1");
    // hello, surfaces.get и surfaces.create первой сессии заняли id 0..=2.
    assert!(
        second.hello_id > first.hello_id + 2,
        "ids continue: {}",
        second.hello_id
    );

    // Регистрация surface идёт после welcome; ждём её.
    tokio::time::timeout(Duration::from_secs(5), async {
        while surfaces.calls().len() < 4 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("surface is registered again");
    assert_eq!(
        surfaces.calls(),
        [
            "surfaces.get",
            "surfaces.create",
            "surfaces.get",
            "surfaces.update"
        ]
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.welcome().session_id.as_str() != "s2" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the handle follows the new connection");

    stop.send(()).unwrap();
    running.await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(5), second.peer.closed())
        .await
        .expect("shutdown closes the connection");
}

#[tokio::test]
async fn connect_waits_for_the_kernel() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("modules.sock");
    let url = Url::parse(&format!("uds://{}", path.display())).unwrap();

    let module = Module::new("late")
        .reconnect_backoff(Duration::from_millis(10), Duration::from_millis(20))
        .connect_wait(Duration::from_secs(5));
    let started = tokio::spawn(async move { module.connect(&url, "token").await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (listener, _) = listen(&dir).await;
    let conn = listener.accept().await.unwrap().upgrade().await.unwrap();
    let kernel = fake::accept(conn, fake::welcome("s1", &[]), Surfaces::default()).await;
    let connected = started
        .await
        .unwrap()
        .expect("connects once the kernel is up");
    assert_eq!(connected.welcome().session_id.as_str(), "s1");
    drop(kernel);
}

#[tokio::test]
async fn gives_up_after_connect_wait() {
    let dir = tempfile::tempdir().unwrap();
    let url = Url::parse(&format!("uds://{}", dir.path().join("none.sock").display())).unwrap();
    let res = Module::new("nobody")
        .reconnect_backoff(Duration::from_millis(10), Duration::from_millis(20))
        .connect_wait(Duration::from_millis(100))
        .connect(&url, "token")
        .await;
    assert!(matches!(res, Err(eva_sdk::ConnectError::Transport(_))));
}

#[tokio::test]
async fn rejected_hello_is_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let (listener, url) = listen(&dir).await;
    tokio::spawn(async move {
        let mut conn = listener.accept().await.unwrap().upgrade().await.unwrap();
        let raw = conn.recv().await.unwrap().unwrap();
        let eva_protocol::wire::Frame::Request(request) =
            eva_rpc::codec::decode_frame(&raw).unwrap()
        else {
            panic!("hello expected");
        };
        let reply = eva_protocol::wire::Frame::Response(eva_protocol::wire::frame::Response::err(
            request.id,
            RpcError::new(ErrorCode::Unauthorized, "bad token"),
        ));
        conn.send(eva_rpc::transport::Outbound::Frame(
            eva_rpc::codec::encode_frame(&reply).unwrap(),
        ))
        .await
        .unwrap();
        std::future::pending::<()>().await;
    });

    let res = Module::new("m")
        .reconnect_backoff(Duration::from_millis(10), Duration::from_millis(20))
        .connect(&url, "wrong")
        .await;
    match res {
        Err(eva_sdk::ConnectError::Rejected(e)) => assert_eq!(e.code, ErrorCode::Unauthorized),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

mod tools {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use futures::StreamExt as _;
    use serde_json::json;
    use tokio::sync::Notify;

    use eva_protocol::{
        types::ChatId,
        wire::tools::{ToolCall, ToolItem, ToolResult},
    };
    use eva_sdk::{ToolContent, ToolCtx, ToolError};

    use super::*;
    use crate::dispatch::EchoInput;

    struct Counters {
        started: AtomicUsize,
        finished: AtomicUsize,
        gate: Notify,
    }

    fn slow(counters: Arc<Counters>) -> Module {
        Module::new("slow")
            .reconnect_backoff(Duration::from_millis(10), Duration::from_millis(40))
            .tool("x.slow", move |_: ToolCtx, _: EchoInput| {
                let counters = counters.clone();
                async move {
                    counters.started.fetch_add(1, Ordering::SeqCst);
                    counters.gate.notified().await;
                    counters.finished.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, ToolError>(ToolResult::ok(vec![ToolContent::text("done")]))
                }
            })
    }

    fn call() -> ToolCall {
        let mut ctx = fake::turn();
        ctx.chat = ChatId(9);
        ToolCall {
            call_id: "toolu_1".to_owned(),
            tool_id: None,
            canonical: "x_slow".to_owned(),
            input: json!({"text": "", "times": 0}),
            ctx,
        }
    }

    async fn wait(what: &str, check: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !check() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    }

    /// Обрыв посреди `tools.call`: тул доживает, повтор с тем же `call_id`
    /// после `hello.resume` получает тот же результат, исполнение одно (`06 §12`).
    #[tokio::test]
    async fn call_survives_a_reconnect_and_runs_once() {
        let dir = tempfile::tempdir().unwrap();
        let (listener, url) = listen(&dir).await;
        let (sessions_tx, mut sessions) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            for session in ["s1", "s2"] {
                let conn = listener.accept().await.unwrap().upgrade().await.unwrap();
                let accepted =
                    fake::accept(conn, fake::welcome(session, &[]), Surfaces::default()).await;
                if sessions_tx.send(accepted).is_err() {
                    return;
                }
            }
            std::future::pending::<()>().await;
        });

        let counters = Arc::new(Counters {
            started: AtomicUsize::new(0),
            finished: AtomicUsize::new(0),
            gate: Notify::new(),
        });
        let connected = slow(counters.clone()).connect(&url, "token").await.unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let running = tokio::spawn(connected.run(async {
            _ = stopped.await;
        }));

        let first = sessions.recv().await.unwrap();
        let module = first.module();
        let pending = tokio::spawn(async move {
            let items: Vec<_> = module.call_tool(&call()).await?.collect().await;
            Ok::<_, RpcError>(items)
        });
        wait("the tool to start", || {
            counters.started.load(Ordering::SeqCst) == 1
        })
        .await;
        first
            .peer
            .close(RpcError::new(ErrorCode::Unavailable, "socket lost"));
        _ = pending.await;

        let second = tokio::time::timeout(Duration::from_secs(5), sessions.recv())
            .await
            .expect("reconnect")
            .unwrap();
        assert!(second.hello.resume.is_some());
        let module = second.module();
        let retry = tokio::spawn(async move {
            let items: Vec<_> = module.call_tool(&call()).await.unwrap().collect().await;
            items
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        counters.gate.notify_waiters();
        let items = retry.await.unwrap();
        match items.last() {
            Some(Ok(ToolItem::Result(result))) => {
                assert_eq!(result.content, vec![ToolContent::text("done")]);
            }
            other => panic!("expected the result, got {other:?}"),
        }
        assert_eq!(counters.started.load(Ordering::SeqCst), 1);
        assert_eq!(counters.finished.load(Ordering::SeqCst), 1);

        stop.send(()).unwrap();
        running.await.unwrap().unwrap();
    }

    /// Отмена ядром на живом соединении прерывает тул.
    #[tokio::test]
    async fn cancel_on_a_live_connection_aborts_the_tool() {
        let dir = tempfile::tempdir().unwrap();
        let (listener, url) = listen(&dir).await;
        let (session_tx, session_rx) = oneshot::channel();
        tokio::spawn(async move {
            let conn = listener.accept().await.unwrap().upgrade().await.unwrap();
            let accepted = fake::accept(conn, fake::welcome("s1", &[]), Surfaces::default()).await;
            _ = session_tx.send(accepted);
            std::future::pending::<()>().await;
        });

        let counters = Arc::new(Counters {
            started: AtomicUsize::new(0),
            finished: AtomicUsize::new(0),
            gate: Notify::new(),
        });
        let _connected = slow(counters.clone()).connect(&url, "token").await.unwrap();
        let session = session_rx.await.unwrap();

        let stream = session.module().call_tool(&call()).await.unwrap();
        wait("the tool to start", || {
            counters.started.load(Ordering::SeqCst) == 1
        })
        .await;
        drop(stream);
        tokio::time::sleep(Duration::from_millis(200)).await;
        counters.gate.notify_waiters();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            counters.finished.load(Ordering::SeqCst),
            0,
            "cancel aborts the tool"
        );

        // Отменённый вызов не кэшируется: повтор исполняется заново.
        let items: Vec<_> = {
            let module = session.module();
            let retry = tokio::spawn(async move {
                module
                    .call_tool(&call())
                    .await
                    .unwrap()
                    .collect::<Vec<_>>()
                    .await
            });
            wait("the retry to start", || {
                counters.started.load(Ordering::SeqCst) == 2
            })
            .await;
            counters.gate.notify_waiters();
            retry.await.unwrap()
        };
        assert!(matches!(items.last(), Some(Ok(ToolItem::Result(_)))));
        assert_eq!(counters.finished.load(Ordering::SeqCst), 1);
    }
}
