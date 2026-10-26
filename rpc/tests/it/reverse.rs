//! Soul → модуль: `tools.call` с прогрессом, хуки, события, провайдер запасного входа.

use std::{sync::Arc, time::Duration};

use futures::StreamExt as _;
use tokio::sync::{Notify, mpsc};

use eva::time::{HumanDuration, Timestamp};
use eva_core::{GenSink, Provider as _};
use eva_protocol::{
    types::{
        ChatId,
        generate::{
            EmbedRequest, EmbedResponse, GenItem, GenerateRequest, ProviderError,
            ProviderErrorKind, Stop, StopReason, Usage,
        },
        model::ProviderDecl,
        tool::ToolContent,
        turn::{Actor, TurnCtx},
    },
    wire::{
        ErrorCode, RpcError,
        capabilities::ProviderCap,
        events::{Event, ModuleEvent, Publish},
        hooks::{AfterTool, BeforeTurn, Decision},
        tools::{ToolCall, ToolItem, ToolResult},
    },
};
use eva_rpc::{ModuleHandler, NoHandler, Progress};

use crate::{
    harness::{Dial, Harness, Via, hello, service_hello},
    soul::CLIENT_TOKEN,
};

struct Module {
    events: mpsc::UnboundedSender<Publish>,
    hung_dropped: Arc<Notify>,
}

struct Flag(Arc<Notify>);

impl Drop for Flag {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

impl ModuleHandler for Module {
    async fn call_tool(&self, call: ToolCall, progress: Progress) -> Result<ToolResult, RpcError> {
        match call.canonical.as_str() {
            "x_fail" => Err(RpcError::new(ErrorCode::Internal, "tool crashed")),
            "x_hang" => {
                let _flag = Flag(self.hung_dropped.clone());
                progress.send("started").await;
                std::future::pending().await
            }
            canonical => {
                progress.send("working").await;
                progress.send("almost").await;
                Ok(ToolResult::ok(vec![ToolContent::text(format!(
                    "done {canonical}"
                ))]))
            }
        }
    }

    async fn after_tool(&self, input: AfterTool) -> Result<Decision<ToolResult>, RpcError> {
        Ok(Decision::Patch {
            payload: ToolResult::ok(vec![ToolContent::text(input.canonical.to_uppercase())]),
        })
    }

    async fn event(&self, event: Publish) {
        _ = self.events.send(event);
    }

    async fn generate(&self, req: GenerateRequest, mut sink: GenSink) -> Result<(), ProviderError> {
        if req.model.as_str() == "broken" {
            return Err(ProviderError {
                kind: ProviderErrorKind::Quota,
                http_status: Some(429),
                raw: "slow down".to_owned(),
                retry_after: Some(HumanDuration(Duration::from_secs(3))),
            });
        }
        for item in [
            GenItem::TextDelta {
                text: "hi".to_owned(),
            },
            GenItem::Stop(Stop {
                reason: StopReason::EndTurn,
                details: None,
            }),
        ] {
            if sink.push(item).await.is_err() {
                break;
            }
        }
        Ok(())
    }

    async fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse, ProviderError> {
        Ok(EmbedResponse {
            vectors: req.inputs.iter().map(|s| vec![s.len() as f32]).collect(),
            usage: Usage::default(),
        })
    }

    async fn count_tokens(&self, _: GenerateRequest) -> Result<u32, ProviderError> {
        Ok(42)
    }
}

/// Как модуль связан с soul: набран им или звонит сам.
#[derive(Debug, Clone, Copy)]
enum Link {
    Dialed(Dial),
    /// Клиент хозяина тоже получает события и хуки.
    Client(Via),
}

struct Connected {
    _h: Harness,
    _client: eva_rpc::RpcClient,
    accepted: eva_rpc::Accepted,
    events: mpsc::UnboundedReceiver<Publish>,
    hung_dropped: Arc<Notify>,
}

async fn connect(link: Link) -> Connected {
    let (events_tx, events) = mpsc::unbounded_channel();
    let hung_dropped = Arc::new(Notify::new());
    let module = Module {
        events: events_tx,
        hung_dropped: hung_dropped.clone(),
    };
    let (h, (client, accepted)) = match link {
        Link::Dialed(dial) => {
            let h = Harness::start(Via::Uds).await;
            let pair = h.dial_with(dial, service_hello(), module).await;
            (h, pair)
        }
        Link::Client(via) => {
            let mut h = Harness::start(via).await;
            let pair = h.connect_with(hello(CLIENT_TOKEN), module).await;
            (h, pair)
        }
    };
    Connected {
        _h: h,
        _client: client,
        accepted,
        events,
        hung_dropped,
    }
}

fn ctx() -> TurnCtx {
    TurnCtx {
        turn_id: "t1".into(),
        chat: ChatId(1),
        actor: Actor::unknown(),
        master_turn: false,
        trusted_chat: false,
        public: true,
        surface: "x".into(),
        anchor: None,
        run_id: None,
    }
}

fn tool_call(canonical: &str) -> ToolCall {
    ToolCall {
        call_id: "call-1".to_owned(),
        tool_id: None,
        canonical: canonical.to_owned(),
        input: serde_json::json!({"text": "hi"}),
        ctx: ctx(),
    }
}

fn request(model: &str) -> GenerateRequest {
    GenerateRequest {
        model: model.into(),
        system: Vec::new(),
        tools: Vec::new(),
        messages: Vec::new(),
        breakpoints: Vec::new(),
        cache: Default::default(),
        effort: None,
        thinking: None,
        max_tokens: 64,
        format: None,
        purpose: Default::default(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_call_with_progress() {
    for link in [
        Link::Dialed(Dial::Uds),
        Link::Dialed(Dial::Tcp),
        Link::Client(Via::Ws),
    ] {
        let c = connect(link).await;
        let module = c.accepted.module();

        let items: Vec<_> = module
            .call_tool(&tool_call("x_send"))
            .await
            .unwrap()
            .collect()
            .await;
        let expected = vec![
            Ok(ToolItem::Progress {
                message: None,
                text: "working".to_owned(),
            }),
            Ok(ToolItem::Progress {
                message: None,
                text: "almost".to_owned(),
            }),
            Ok(ToolItem::Result(ToolResult::ok(vec![ToolContent::text(
                "done x_send",
            )]))),
        ];
        assert_eq!(items, expected, "{link:?}");

        // Ошибка потока — `Fatal` тура: код доезжает до soul.
        let items: Vec<_> = module
            .call_tool(&tool_call("x_fail"))
            .await
            .unwrap()
            .collect()
            .await;
        assert_eq!(items.len(), 1, "{link:?}");
        assert_eq!(items[0].as_ref().unwrap_err().code, ErrorCode::Internal);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_call_cancel_reaches_the_module() {
    let c = connect(Link::Dialed(Dial::Uds)).await;
    let mut items = c
        .accepted
        .module()
        .call_tool(&tool_call("x_hang"))
        .await
        .unwrap();
    assert_eq!(
        items.next().await,
        Some(Ok(ToolItem::Progress {
            message: None,
            text: "started".to_owned(),
        }))
    );
    drop(items);

    tokio::time::timeout(Duration::from_secs(5), c.hung_dropped.notified())
        .await
        .expect("the tool future must be dropped on cancel");
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_call_without_handler() {
    let h = Harness::start(Via::Uds).await;
    let (_client, accepted) = h.dial_with(Dial::Uds, service_hello(), NoHandler).await;
    // Тул исполняется внутри потока: отказ — его единственный элемент.
    let items: Vec<_> = accepted
        .module()
        .call_tool(&tool_call("x_send"))
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].as_ref().unwrap_err().code,
        ErrorCode::UnknownMethod
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn hooks() {
    let c = connect(Link::Dialed(Dial::Uds)).await;
    let module = c.accepted.module();

    let decision = module
        .hook(
            &AfterTool {
                ctx: ctx(),
                tool_id: None,
                canonical: "hello".to_owned(),
                input: serde_json::json!({}),
                result: ToolResult::ok(Vec::new()),
            },
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    assert_eq!(
        decision,
        Decision::Patch {
            payload: ToolResult::ok(vec![ToolContent::text("HELLO")]),
        }
    );

    let err = module
        .hook(
            &BeforeTurn {
                ctx: ctx(),
                content: Vec::new(),
            },
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::UnknownMethod);
}

#[tokio::test(flavor = "multi_thread")]
async fn events_keep_order() {
    for link in [Link::Dialed(Dial::Tcp), Link::Client(Via::Ws)] {
        events_in_order(connect(link).await).await;
    }
}

async fn events_in_order(mut c: Connected) {
    let module = c.accepted.module();

    for seq in 0..20 {
        module
            .publish(&Publish {
                seq,
                at: Timestamp::from_secs(1_750_000_000 + seq),
                event: Event::ModuleConnected(ModuleEvent {
                    module: format!("m{seq}").into(),
                }),
            })
            .await
            .unwrap();
    }

    for seq in 0..20 {
        let event = tokio::time::timeout(Duration::from_secs(5), c.events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.seq, seq);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn module_provider() {
    let c = connect(Link::Dialed(Dial::Uds)).await;
    let provider = c.accepted.module().provider(ProviderCap {
        decl: ProviderDecl {
            local: false,
            count_tokens: true,
        },
        models: Vec::new(),
    });
    assert!(provider.decl().count_tokens);

    let (tx, rx) = futures::channel::mpsc::channel(16);
    provider
        .generate(request("m"), GenSink::new(tx))
        .await
        .unwrap();
    let items: Vec<GenItem> = rx.collect().await;
    assert_eq!(
        items,
        vec![
            GenItem::TextDelta {
                text: "hi".to_owned(),
            },
            GenItem::Stop(Stop {
                reason: StopReason::EndTurn,
                details: None,
            }),
        ]
    );

    let (tx, _rx) = futures::channel::mpsc::channel(16);
    let err = provider
        .generate(request("broken"), GenSink::new(tx))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ProviderErrorKind::Quota);
    assert_eq!(err.http_status, Some(429));
    assert_eq!(err.retry_after, Some(HumanDuration(Duration::from_secs(3))));

    let embedded = provider
        .embed(EmbedRequest {
            model: "e".into(),
            inputs: vec!["ab".to_owned(), "abc".to_owned()],
        })
        .await
        .unwrap();
    assert_eq!(embedded.vectors, vec![vec![2.0], vec![3.0]]);

    assert_eq!(provider.count_tokens(request("m")).await.unwrap(), 42);

    // Обрыв сессии посреди работы — `transient`: fallback-цепочка soul повторит.
    c.accepted
        .peer
        .close(RpcError::new(ErrorCode::Unavailable, "gone"));
    let err = provider.count_tokens(request("m")).await.unwrap_err();
    assert_eq!(err.kind, ProviderErrorKind::Transient);
}
