//! Вызовы ядро → модуль через настоящий `ModuleClient`; handshake без регистрации surface.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use eva::{data, time::Timestamp};
use futures::StreamExt as _;
use serde_json::json;
use tokio::sync::{Notify, mpsc};

use eva_core::prelude::*;
use eva_protocol::{
    requests::surfaces::create,
    types::{ChatId, SurfaceId, ToolId},
    wire::{
        ErrorCode, RpcError,
        events::{ChatEvent, Event, ModuleEvent, Publish, Topic},
        hooks::{BeforeSend, BeforeTurn, Decision, HookPoint},
        tools::{ToolCall, ToolItem, ToolResult},
    },
};
use eva_rpc::{Config, Conn};
use eva_sdk::{Connected, Module, ToolContent, ToolCtx, ToolError};

use crate::fake::{self, Surfaces};

#[data]
pub struct EchoInput {
    pub text: String,
    pub times: u32,
}

/// Модуль и поддельное ядро на паре соединений в памяти.
async fn connect(
    module: Module,
    welcome: eva_protocol::wire::Welcome,
) -> (Connected, fake::Session, Surfaces) {
    let (module_end, kernel_end) = Conn::pair(&Config::default());
    let surfaces = Surfaces::default();
    let kernel = tokio::spawn(fake::accept(kernel_end, welcome, surfaces.clone()));
    let connected = module
        .handshake(module_end, &fake::token("token"))
        .await
        .expect("handshake");
    let session = kernel.await.expect("fake kernel");
    (connected, session, surfaces)
}

fn call(
    call_id: &str,
    tool_id: Option<i64>,
    canonical: &str,
    input: serde_json::Value,
) -> ToolCall {
    ToolCall {
        call_id: call_id.to_owned(),
        tool_id: tool_id.map(ToolId),
        canonical: canonical.to_owned(),
        input,
        ctx: fake::turn(),
    }
}

/// Все элементы потока `tools.call`.
async fn run_tool(session: &fake::Session, call: &ToolCall) -> Vec<Result<ToolItem, RpcError>> {
    match session.module().call_tool(call).await {
        Ok(items) => items.collect().await,
        Err(e) => vec![Err(e)],
    }
}

fn result(items: &[Result<ToolItem, RpcError>]) -> ToolResult {
    match items.last() {
        Some(Ok(ToolItem::Result(result))) => result.clone(),
        other => panic!("expected a result, got {other:?}"),
    }
}

fn text(result: &ToolResult) -> &str {
    match result.content.first() {
        Some(ToolContent::Text { text }) => text,
        other => panic!("expected text, got {other:?}"),
    }
}

fn echo_module(runs: Arc<AtomicUsize>) -> Module {
    Module::new().tool("x.echo", move |ctx: ToolCtx, input: EchoInput| {
        let runs = runs.clone();
        async move {
            runs.fetch_add(1, Ordering::SeqCst);
            ctx.progress(format!("echo for {}", ctx.call_id())).await;
            Ok::<_, ToolError>(ToolResult::ok(vec![ToolContent::text(
                input.text.repeat(input.times as usize),
            )]))
        }
    })
}

#[tokio::test]
async fn tools_dispatch_by_id_and_canonical() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (_connected, session, _) = connect(
        echo_module(runs.clone()),
        fake::welcome("s1", &[("x.echo", 7)]),
    )
    .await;

    let items = run_tool(
        &session,
        &call(
            "c1",
            Some(7),
            "renamed_by_sleep",
            json!({"text": "a", "times": 2}),
        ),
    )
    .await;
    assert!(matches!(&items[0], Ok(ToolItem::Progress { text }) if text == "echo for c1"));
    assert_eq!(text(&result(&items)), "aa");

    // Модель пишет число строкой — приводится по схеме.
    let items = run_tool(
        &session,
        &call("c2", None, "x_echo", json!({"text": "b", "times": "3"})),
    )
    .await;
    assert_eq!(text(&result(&items)), "bbb");
    assert_eq!(runs.load(Ordering::SeqCst), 2);

    let items = run_tool(&session, &call("c3", None, "x_echo", json!({"text": "b"}))).await;
    let res = result(&items);
    assert!(res.is_error);
    assert!(text(&res).starts_with("invalid input"), "{}", text(&res));

    let items = run_tool(&session, &call("c4", Some(99), "x_missing", json!({}))).await;
    match items.as_slice() {
        [Err(e)] => assert_eq!(e.code, ErrorCode::NotFound),
        other => panic!("unknown tool must fail the call, got {other:?}"),
    }
}

#[tokio::test]
async fn repeated_call_id_runs_once() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (_connected, session, _) =
        connect(echo_module(runs.clone()), fake::welcome("s1", &[])).await;

    let first = call("same", None, "x_echo", json!({"text": "z", "times": 1}));
    let a = result(&run_tool(&session, &first).await);
    let b = result(&run_tool(&session, &first).await);
    assert_eq!(a, b);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_duplicates_share_one_execution() {
    let runs = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Notify::new());
    let module = {
        let (runs, gate) = (runs.clone(), gate.clone());
        Module::new().tool("x.slow", move |_: ToolCtx, _: EchoInput| {
            let (runs, gate) = (runs.clone(), gate.clone());
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                gate.notified().await;
                Ok::<_, ToolError>(ToolResult::ok(vec![ToolContent::text("done")]))
            }
        })
    };
    let (_connected, session, _) = connect(module, fake::welcome("s1", &[])).await;

    let the_call = call("dup", None, "x_slow", json!({"text": "", "times": 0}));
    let first = run_tool(&session, &the_call);
    let second = run_tool(&session, &the_call);
    let release = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        gate.notify_waiters();
    };
    let (a, b, ()) = tokio::join!(first, second, release);
    assert_eq!(text(&result(&a)), "done");
    assert_eq!(result(&a), result(&b));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn fatal_error_ends_the_stream() {
    let module = Module::new().tool("x.fail", |_: ToolCtx, _: EchoInput| async {
        Err::<ToolResult, _>(ToolError::fatal("disk on fire"))
    });
    let (_connected, session, _) = connect(module, fake::welcome("s1", &[])).await;
    let items = run_tool(
        &session,
        &call("f", None, "x_fail", json!({"text": "", "times": 0})),
    )
    .await;
    match items.as_slice() {
        [Err(e)] => assert!(e.message.contains("disk on fire")),
        other => panic!("expected a fatal error, got {other:?}"),
    }

    // Обычная ошибка через `?` — данные для модели, не обрыв тура.
    let module = Module::new().tool("x.soft", |_: ToolCtx, _: EchoInput| async {
        Err::<ToolResult, _>("not today")
    });
    let (_connected, session, _) = connect(module, fake::welcome("s2", &[])).await;
    let res = result(
        &run_tool(
            &session,
            &call("s", None, "x_soft", json!({"text": "", "times": 0})),
        )
        .await,
    );
    assert!(res.is_error);
    assert_eq!(text(&res), "not today");
}

/// Паника тула — `internal`, а не `cancelled`; повтор того же `call_id`
/// получает тот же итог и тул второй раз не зовёт.
#[tokio::test]
async fn a_panicking_tool_ends_internal_and_is_not_rerun() {
    let runs = Arc::new(AtomicUsize::new(0));
    let module = {
        let runs = runs.clone();
        Module::new().tool("x.boom", move |_: ToolCtx, _: EchoInput| {
            let runs = runs.clone();
            async move {
                if runs.fetch_add(1, Ordering::SeqCst) < usize::MAX {
                    panic!("tool bug");
                }
                Ok::<_, ToolError>(ToolResult::ok(Vec::new()))
            }
        })
    };
    let (_connected, session, _) = connect(module, fake::welcome("s1", &[])).await;

    let the_call = call("p", None, "x_boom", json!({"text": "", "times": 0}));
    for attempt in 0..2 {
        let items = tokio::time::timeout(Duration::from_secs(5), run_tool(&session, &the_call))
            .await
            .expect("the call ends");
        match items.as_slice() {
            [Err(e)] => assert_eq!(e.code, ErrorCode::Internal, "attempt {attempt}: {e:?}"),
            other => panic!("attempt {attempt}: expected internal, got {other:?}"),
        }
    }
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn events_reach_handlers_of_their_topic() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let module = Module::new().on_event(Topic::ChatCreated, move |_, event| {
        let tx = tx.clone();
        async move {
            _ = tx.send(event);
        }
    });
    let (_connected, session, _) = connect(module, fake::welcome("s1", &[])).await;

    let at = Timestamp::from_secs(1_750_000_000);
    let other = Event::ModuleConnected(ModuleEvent { module: "x".into() });
    let chat = Event::ChatCreated(ChatEvent {
        chat: ChatId(5),
        surface: SurfaceId::from("x"),
        external_id: None,
    });
    for (seq, event) in [other, chat.clone()].into_iter().enumerate() {
        session
            .module()
            .publish(&Publish {
                seq: seq as u64,
                at,
                event,
            })
            .await
            .unwrap();
    }

    let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got, chat);
    assert!(
        rx.try_recv().is_err(),
        "a foreign topic must not be delivered"
    );
}

#[tokio::test]
async fn hooks_answer_only_declared_points() {
    let module = Module::new().hook(HookPoint::BeforeSend, |_, input: BeforeSend| async move {
        Decision::Patch {
            payload: input.text.to_uppercase(),
        }
    });
    let (_connected, session, _) = connect(module, fake::welcome("s1", &[])).await;

    let decision = session
        .module()
        .hook(
            &BeforeSend {
                ctx: fake::turn(),
                text: "hi".to_owned(),
                attachments: Vec::new(),
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(
        decision,
        Decision::Patch {
            payload: "HI".to_owned()
        }
    );

    let undeclared = session
        .module()
        .hook(
            &BeforeTurn {
                ctx: fake::turn(),
                content: Vec::new(),
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert_eq!(undeclared.code, ErrorCode::UnknownMethod);
}

/// Surface модуль создаёт сам: сам по себе handshake в `surfaces.*` не ходит,
/// `ask` уезжает capability, id модуля — из `welcome`.
#[tokio::test]
async fn the_handshake_registers_nothing_and_carries_ask() {
    let (connected, session, surfaces) =
        connect(Module::new().ask(true), fake::welcome("s1", &[])).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(surfaces.calls().is_empty());
    assert!(session.hello.capabilities.ask);
    assert_eq!(connected.welcome().session_id.as_str(), "s1");
    assert_eq!(connected.module().as_str(), fake::MODULE);
    assert_eq!(connected.caller().label(), fake::MODULE);
}

/// Создание — обычный вызов `Kernel`, повторять его на каждом старте безопасно.
#[tokio::test]
async fn the_module_creates_its_surface_through_the_kernel() {
    let (connected, _session, surfaces) = connect(Module::new(), fake::welcome("s1", &[])).await;
    let created = connected
        .kernel()
        .surfaces()
        .create()
        .call(create::Args {
            id: SurfaceId::from("x"),
            name: "Messenger X".to_owned(),
            rules: Some("be brief".to_owned()),
        })
        .await
        .expect("surfaces.create");
    assert_eq!(created.surface.name, "Messenger X");
    assert_eq!(surfaces.calls(), ["surfaces.create"]);
}
