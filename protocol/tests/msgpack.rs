//! msgpack-провод (`to_vec_named`): roundtrip wire-типов и терпимость к
//! вариантам пира новее (`06 §1, §12`).

mod common;

use std::{collections::BTreeMap, fmt::Debug, time::Duration};

use eva::time::{HumanDuration, Timestamp};
use serde::{Serialize, de::DeserializeOwned};
use serde_bytes::ByteBuf;
use serde_json::{Value, json};

use eva_protocol::{
    errors::{self, Generic},
    requests::{Blobs, Cell, Chats, Folders, Patch, Selector, Sqlite, Statement, Surfaces},
    types::{
        Bytes, ChatId, Effort, EventId, FolderId, MemoryId, Thinking, ToolId, Value as Param,
        blob::{self, Meta},
        content, folder,
        generate::{
            Block, CacheMode, ClearAt, ContextEdits, GenItem, GenerateRequest, Message,
            OutputConfig, OutputFormat, Position, ProviderErrorKind, Purpose, RateLimits,
            ResultBlock, Stop, StopReason, SystemKind, SystemSection, ToolChanges, ToolSpec, Usage,
        },
        model::{EmbedDecl, ModelCaps, ModelDecl, ModelEntry, Price, ProviderDecl},
        module::{ModuleKind, ServiceAddress},
        sleep::Phase,
        tool::{GroupDecl, Need, ResultForm, ToolContent, ToolDecl},
        turn::{
            Actor, ActorKind, Decision as Button, FeedEvent, Item, ItemBody, ItemDelta, ItemStatus,
            PendingKind, PendingOutcome, PlanTask, StepUsage, TaskStatus, ToolCallResult, TurnCtx,
            TurnItem, TurnOutcome,
        },
    },
    wire::{
        Capabilities, ErrorCode, Frame, Hello, RpcError, Welcome,
        capabilities::{EventsCap, FailMode, HooksCap, ProviderCap, ToolsCap},
        events::{
            ChatEvent, ChatPurged, CronFired, Event, MemoryChanged, MemoryOp, ModuleEvent,
            NotifyEvent, PhaseState, Publish, SleepPhase, ToolCalled, Topic, TurnEvent,
        },
        frame::{Cancel, Notify, Request, Response, StreamEnd, StreamHeartbeat, StreamItem},
        handshake::{Ping, ProtoVersion, Resume, Warning},
        hooks::{AfterTool, BeforeTool, BeforeTurn, Decision, Hook, HookPoint},
        provider::TokenCount,
        tools::{ToolCall, ToolItem, ToolResult},
    },
};

fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    rmp_serde::to_vec_named(value).expect("encode")
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> T {
    rmp_serde::from_slice(bytes).unwrap_or_else(|e| panic!("{}: {e}", std::any::type_name::<T>()))
}

fn roundtrip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T) {
    let back: T = decode(&encode(value));
    assert_eq!(&back, value);
}

fn at() -> Timestamp {
    Timestamp::from_secs(1_750_000_000)
}

fn secs(n: u64) -> HumanDuration {
    HumanDuration(Duration::from_secs(n))
}

fn ctx() -> TurnCtx {
    TurnCtx {
        turn_id: "0198-turn".into(),
        chat: ChatId(42),
        actor: Actor {
            person_id: Some("p1".into()),
            kind: ActorKind::Master,
            name: Some("Master".into()),
            handle: Some("@m".into()),
        },
        master_turn: true,
        trusted_chat: true,
        public: false,
        surface: "x".into(),
        anchor: Some(json!({"message": 17, "thread": [1, 2]})),
        run_id: None,
    }
}

fn image() -> ToolContent {
    ToolContent::Image {
        media_type: "image/png".into(),
        data: Bytes::from([0x89, b'P', b'N', b'G', 0, 1, 2]),
    }
}

fn tool_decl() -> ToolDecl {
    ToolDecl {
        path: "x.send".into(),
        description: "Send a message".into(),
        input: json!({"type": "object", "properties": {"chat": {"type": "string"}}}),
        master_only: true,
        trusted_only: false,
        needs: vec![Need::Private, Need::AskUi],
        ends_turn: true,
        parallel_safe: false,
        waits_on_cancel: true,
        result: ResultForm::Uncapped,
        timeout: Some(secs(30)),
        idempotent: false,
    }
}

#[test]
fn tool_execution_flags_default_to_true_and_allow_explicit_false() {
    let mut value = json!({
        "path": "weather.current", "description": "Get current weather", "input": {}
    });
    let defaults: ToolDecl = decode(&encode(&value));
    assert!(defaults.idempotent && defaults.parallel_safe);
    value["idempotent"] = json!(false);
    value["parallel_safe"] = json!(false);
    let overridden: ToolDecl = decode(&encode(&value));
    assert!(!overridden.idempotent && !overridden.parallel_safe);
    roundtrip(&defaults);
    roundtrip(&overridden);
}

#[test]
fn frame_every_variant() {
    let params = Chats::delete::Args {
        chat: ChatId(7),
        purge: true,
    };
    let error = RpcError {
        code: ErrorCode::Quota,
        message: "quota: rows".into(),
        data: Some(encode(&"opaque").into()),
        retry_after: Some(secs(5)),
    };
    let frames = [
        Frame::Request(Request {
            id: 1,
            method: "chats.delete".into(),
            params: encode(&params).into(),
        }),
        Frame::Response(Response::ok(1, encode(&Chats::delete::Ok {}).into())),
        Frame::Response(Response::err(2, error.clone())),
        Frame::Notify(Notify {
            method: "ping".into(),
            params: encode(&Ping {}).into(),
        }),
        Frame::StreamItem(StreamItem {
            id: 3,
            seq: 0,
            item: encode(&GenItem::TextDelta { text: "hi".into() }).into(),
        }),
        Frame::StreamHeartbeat(StreamHeartbeat { id: 3 }),
        Frame::StreamEnd(StreamEnd { id: 3, error: None }),
        Frame::StreamEnd(StreamEnd {
            id: 4,
            error: Some(RpcError::cancelled()),
        }),
        Frame::Cancel(Cancel { id: 4 }),
        Frame::Unknown,
    ];
    common::assert_every_variant(&frames);
    for frame in &frames {
        roundtrip(frame);
    }

    // Второй шаг разбора: payload кадра — msgpack `Args`.
    let Frame::Request(request) = decode(&encode(&frames[0])) else {
        panic!("request expected");
    };
    assert_eq!(decode::<Chats::delete::Args>(&request.params), params);
    // На проводе `params` — `bin` с msgpack `Args`, не вложенная карта.
    let Param::Map(outer) = decode::<Param>(&encode(&frames[0])) else {
        panic!("frame is a map");
    };
    let [(tag, Param::Map(fields))] = outer.as_slice() else {
        panic!("one variant key: {outer:?}");
    };
    assert_eq!(tag, &Param::from("request"));
    let payload = fields.iter().find(|(key, _)| key == &Param::from("params"));
    assert_eq!(
        payload.map(|(_, value)| value),
        Some(&Param::Bytes(encode(&params).into()))
    );
    assert_eq!(frames[0].id(), Some(1));
    assert_eq!(frames[3].id(), None);

    let Frame::Response(response) = decode(&encode(&frames[2])) else {
        panic!("response expected");
    };
    assert_eq!(response.into_result(), Err(error));
    let malformed = Response {
        id: 9,
        result: None,
        error: None,
    };
    assert_eq!(
        malformed.into_result().map_err(|e| e.code),
        Err(ErrorCode::InvalidParams)
    );
}

/// `06 §12`: кадр с лишними полями жив.
#[test]
fn frame_extra_fields() {
    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    enum Newer {
        Request {
            id: u64,
            method: String,
            params: ByteBuf,
            trace_id: String,
            deadline: Option<u64>,
        },
    }

    let frame: Frame = decode(&encode(&Newer::Request {
        id: 5,
        method: "chats.get".into(),
        params: ByteBuf::from(encode(&json!({"chat": 1}))),
        trace_id: "t".into(),
        deadline: Some(3),
    }));
    let Frame::Request(request) = frame else {
        panic!("request expected");
    };
    assert_eq!((request.id, request.method.as_str()), (5, "chats.get"));
}

#[test]
fn handshake() {
    let capabilities = Capabilities {
        provider: Some(ProviderCap {
            decl: ProviderDecl {
                local: false,
                count_tokens: true,
            },
            models: vec![ModelDecl {
                name: "vendor/model".into(),
                context_window: 200_000,
                max_output: 8192,
                caps: ModelCaps {
                    vision: true,
                    thinking: true,
                    effort_levels: vec![Effort::Low, Effort::High],
                    cache_breakpoints: 4,
                    cache_min_prefix: 1024,
                    ..ModelCaps::default()
                },
                price: Some(Price {
                    input: 3.0,
                    output: 15.0,
                    cache_read: 0.3,
                    cache_write_short: 3.75,
                    cache_write_long: 6.0,
                }),
                embed: Some(EmbedDecl {
                    dims: 1024,
                    max_input_tokens: 8192,
                }),
            }],
        }),
        tools: Some(ToolsCap {
            groups: vec![GroupDecl {
                path: "x".into(),
                about: "Messenger X actions".into(),
            }],
            tools: vec![tool_decl()],
        }),
        events: Some(EventsCap {
            subscriptions: vec![Topic::ChatDeleted, Topic::TurnFinished],
        }),
        hooks: Some(HooksCap {
            points: vec![HookPoint::BeforeTool, HookPoint::AfterTool],
            timeout: Some(HumanDuration(Duration::from_millis(1500))),
            fail_mode: FailMode::Closed,
        }),
        ask: true,
    };
    assert_eq!(
        capabilities.names(),
        ["provider", "tools", "events", "hooks", "ask"]
    );
    let absent: Capabilities = decode(&encode(&json!({})));
    assert!(!absent.ask, "ask defaults to false");

    let hello = Hello {
        version: "1.2.3".into(),
        proto_version: ProtoVersion::CURRENT,
        token: Some("secret".into()),
        capabilities,
        resume: Some(Resume {
            session_id: "0198-session".into(),
        }),
    };
    roundtrip(&hello);
    assert!(
        !format!("{hello:?}").contains("secret"),
        "token leaks into Debug"
    );
    let wire: serde_json::Value = rmp_serde::from_slice(&encode(&hello)).expect("hello as a map");
    assert!(
        wire.get("module").is_none(),
        "identity comes from the token: {wire}"
    );
    // Сервис, которому звонит soul, токена не шлёт вовсе.
    let dialed: Hello = decode(&encode(&json!({
        "version": "1.2.3",
        "proto_version": "0.1",
    })));
    assert_eq!(dialed.token, None);
    roundtrip(&Capabilities::default());

    let welcome = Welcome {
        session_id: "0198-session".into(),
        module: "messenger-x".into(),
        soul_version: "0.1.0".into(),
        proto_version: ProtoVersion { major: 0, minor: 1 },
        kind: ModuleKind::Service,
        services: vec!["chats".into(), "turns".into()],
        tool_ids: BTreeMap::from([("x.send".into(), ToolId(17))]),
        models_accepted: vec!["vendor/model".into()],
        warnings: vec![Warning {
            code: ErrorCode::Forbidden,
            subject: "provider".into(),
            message: "module is not named in llm.providers".into(),
        }],
        resumed: true,
    };
    roundtrip(&welcome);

    // Пустые секции `welcome` опциональны на проводе.
    let minimal: Welcome = decode(&encode(&json!({
        "session_id": "s",
        "module": "phone",
        "soul_version": "0.1.0",
        "proto_version": "0.1",
        "kind": "client",
    })));
    assert!(minimal.tool_ids.is_empty() && minimal.warnings.is_empty());
    assert!(
        !minimal.resumed,
        "an older soul never claims a resumed window"
    );

    assert_eq!(
        ProtoVersion::parse("2.10"),
        Some(ProtoVersion {
            major: 2,
            minor: 10
        })
    );
    assert_eq!(ProtoVersion::parse("2"), None);
    assert_eq!(encode(&ProtoVersion { major: 1, minor: 4 }), encode(&"1.4"));
}

#[test]
fn service_address() {
    for (text, address) in [
        (
            "uds:///run/eva-bridge/module.sock",
            ServiceAddress::Uds("/run/eva-bridge/module.sock".into()),
        ),
        (
            "tcp://bridge.lan:7000",
            ServiceAddress::Tcp {
                host: "bridge.lan".into(),
                port: 7000,
            },
        ),
        (
            "tcp://10.0.0.2:1",
            ServiceAddress::Tcp {
                host: "10.0.0.2".into(),
                port: 1,
            },
        ),
        (
            "tcp://[::1]:65535",
            ServiceAddress::Tcp {
                host: "::1".into(),
                port: 65535,
            },
        ),
    ] {
        let parsed: ServiceAddress = text.parse().unwrap();
        assert_eq!(parsed, address);
        assert_eq!(parsed.to_string(), text);
        roundtrip(&parsed);
        assert_eq!(serde_json::to_value(&parsed).unwrap(), json!(text));
    }

    for bad in [
        "uds://run/x.sock",
        "uds://",
        "unix:///run/x.sock",
        "ws://host:1",
        "tcp://host",
        "tcp://host:",
        "tcp://host:0",
        "tcp://host:65536",
        "tcp://:7000",
        "tcp://host:7000/",
        "tcp://host:7000?x=1",
        "tcp://host:7000#f",
        "tcp://user@host:7000",
        "tcp://::1:7000",
        "tcp://[host]:7000",
        "tcp://a b:7000",
    ] {
        assert!(
            ServiceAddress::parse(bad).is_err(),
            "{bad} must be rejected"
        );
        assert!(serde_json::from_value::<ServiceAddress>(json!(bad)).is_err());
    }
}

#[test]
fn generate_request() {
    let req = GenerateRequest {
        model: "vendor/model".into(),
        system: vec![SystemSection {
            name: "rules".into(),
            text: "Be brief.".into(),
        }],
        tools: vec![ToolSpec {
            name: "memory_recall".into(),
            description: "Recall memory".into(),
            input_schema: json!({"type": "object", "properties": {}}),
            deferred: true,
        }],
        messages: vec![
            Message::User {
                blocks: vec![
                    Block::Text {
                        text: "hello".into(),
                    },
                    Block::Image {
                        media_type: "image/jpeg".into(),
                        data: Bytes::from([0xff, 0xd8, 0xff]),
                    },
                    Block::ToolResult {
                        tool_use_id: "call-1".into(),
                        content: vec![
                            ResultBlock::Text { text: "ok".into() },
                            ResultBlock::Image {
                                media_type: "image/png".into(),
                                data: Bytes::from([1, 2]),
                            },
                        ],
                        is_error: true,
                    },
                ],
            },
            Message::Assistant {
                blocks: vec![
                    Block::Thinking {
                        signature: Bytes::from([7; 64]),
                        model: "vendor/model".into(),
                        text: Some("hmm".into()),
                    },
                    Block::RedactedThinking {
                        data: Bytes::from([9; 33]),
                        model: "vendor/model".into(),
                    },
                    Block::ToolUse {
                        id: "call-2".into(),
                        name: "memory_recall".into(),
                        input: json!({"query": "tea", "limit": 3, "exact": false}),
                    },
                ],
            },
            Message::System {
                kind: SystemKind::Reminder,
                blocks: vec![Block::Text {
                    text: "[soul] 12:00".into(),
                }],
                clear_at: Some(ClearAt::NextUserMessage),
                output_config: Some(OutputConfig {
                    effort: Effort::High,
                }),
                tool_changes: Some(ToolChanges {
                    added: vec!["notes_save".into()],
                }),
            },
        ],
        breakpoints: vec![Position::System, Position::Message { index: 1 }],
        cache: CacheMode::Long,
        effort: Some(Effort::Max),
        thinking: Some(Thinking::Adaptive),
        max_tokens: 4096,
        format: Some(OutputFormat {
            schema: json!({"type": "object"}),
            strict: true,
        }),
        purpose: Purpose::Compaction,
    };
    roundtrip(&req);
    roundtrip(&TokenCount { tokens: 1234 });
}

#[test]
fn gen_item_every_variant() {
    let items = [
        GenItem::TextDelta { text: "he".into() },
        GenItem::ThinkingDelta {
            text: String::new(),
        },
        GenItem::ThinkingBlock {
            signature: Bytes::from([1; 300]),
            model: "m".into(),
        },
        GenItem::RedactedThinking {
            data: Bytes::from([2; 70_000]),
            model: "m".into(),
        },
        GenItem::ToolUse {
            id: "call".into(),
            name: "x_send".into(),
            input: json!({"chat": "1", "nested": {"a": [1, 2.5, null]}}),
        },
        GenItem::Image {
            media_type: "image/webp".into(),
            data: Bytes::from([3; 10]),
        },
        GenItem::Usage(Usage {
            input: 10,
            cache_read: 20,
            cache_write_short: 30,
            cache_write_long: 40,
            output: 50,
            reasoning: 60,
            context_edits: ContextEdits {
                applied: 1,
                cleared_input_tokens: 500,
            },
            cost: Some(0.25),
            rate_limits: Some(RateLimits {
                requests_remaining: Some(99),
                tokens_remaining: None,
                reset_at: Some(at()),
            }),
        }),
        GenItem::Stop(Stop {
            reason: StopReason::MaxTokens,
            details: Some("length".into()),
        }),
        GenItem::Unknown,
    ];
    common::assert_every_variant(&items);
    for item in &items {
        roundtrip(item);
    }
}

#[test]
fn turn_item() {
    let item = |status, body| Item {
        id: "0198-turn:0".into(),
        turn: Some("0198-turn".into()),
        seq: EventId(100),
        status,
        body,
    };
    let task = PlanTask {
        id: 1,
        phase: Some("build".into()),
        title: "Write tests".into(),
        status: TaskStatus::InProgress,
        outcome: None,
    };
    let bodies = [
        ItemBody::UserMessage {
            text: "hi".into(),
            sender: Some("p1".into()),
            master: true,
            external_id: Some("0199-optimistic".into()),
        },
        ItemBody::AgentMessage {
            text: "hello".into(),
            cost: Some(0.01),
        },
        ItemBody::Thinking { text: "hmm".into() },
        ItemBody::ToolCall {
            call_id: "call".into(),
            name: "x_send".into(),
            input: json!({"chat": 1}),
            agent: Some("scout".into()),
            result: Some(ToolCallResult {
                output: vec![ToolContent::text("sent"), image()],
                is_error: false,
            }),
            progress: Some("50%".into()),
        },
        ItemBody::ClientOp {
            pending: "0198-pending".into(),
            kind: PendingKind::Permission,
            payload: json!({"command": "ls"}),
            decisions: vec![Button::Allow, Button::AllowSession, Button::Deny],
            resolution: Some(PendingOutcome::Allowed),
        },
        ItemBody::Image {
            blob: "0198-blob".into(),
            url: "eva-blob://0198-blob".into(),
            name: "cat.png".into(),
            media_type: "image/png".into(),
        },
        ItemBody::File {
            blob: "0198-document".into(),
            url: "eva-blob://0198-document".into(),
            name: "notes.txt".into(),
            media_type: "text/plain".into(),
        },
        ItemBody::Compaction {
            summary_ref: 3.into(),
        },
        ItemBody::Reminder {
            text: "[soul] 12:00".into(),
        },
        ItemBody::Plan {
            tasks: vec![task.clone()],
        },
        ItemBody::Unknown,
    ];
    common::assert_every_variant(&bodies);

    let mut events: Vec<FeedEvent> = bodies
        .into_iter()
        .map(|body| FeedEvent::ItemCompleted {
            item: item(ItemStatus::Completed, body),
            replaces: Some("0198-turn:1:a".into()),
        })
        .collect();
    events.extend([
        FeedEvent::ItemStarted {
            item: item(
                ItemStatus::Running,
                ItemBody::Thinking {
                    text: String::new(),
                },
            ),
        },
        FeedEvent::ItemDelta {
            id: "call".into(),
            delta: ItemDelta::Progress {
                message: None,
                text: "1/3".into(),
            },
        },
        FeedEvent::ItemDelta {
            id: "0198-turn:1".into(),
            delta: ItemDelta::Text { text: "wor".into() },
        },
        FeedEvent::ItemDelta {
            id: "plan".into(),
            delta: ItemDelta::Plan { tasks: vec![task] },
        },
        FeedEvent::RequestResolved {
            pending: "0198-pending".into(),
            outcome: PendingOutcome::Expired,
        },
        FeedEvent::Usage(StepUsage {
            turn: "0198-turn".into(),
            step: 2,
            model: "m".into(),
            input: 1,
            cache_read: 2,
            cache_write: 3,
            output: 4,
            cost: None,
        }),
        FeedEvent::TurnDone {
            turn: "0198-turn".into(),
            outcome: TurnOutcome::Failed,
            error: Some("provider failed".into()),
        },
        FeedEvent::ItemDelta {
            id: "future".into(),
            delta: ItemDelta::Unknown,
        },
        FeedEvent::Unknown,
    ]);
    common::assert_every_variant(&events);
    let deltas = events.iter().filter_map(|event| match event {
        FeedEvent::ItemDelta { delta, .. } => Some(delta),
        _ => None,
    });
    common::assert_every_variant(deltas);

    for (seq, event) in events.into_iter().enumerate() {
        roundtrip(&TurnItem {
            seq: EventId(200 + seq as i64),
            sub: 1,
            replayed: seq % 2 == 0,
            event,
        });
    }
}

#[test]
fn tools_call() {
    for content in [
        ToolContent::text("done"),
        image(),
        ToolContent::Blob {
            id: "0198-blob".into(),
        },
        ToolContent::Unknown,
    ] {
        roundtrip(&content);
    }

    roundtrip(&ToolCall {
        call_id: "call".into(),
        tool_id: Some(ToolId(17)),
        canonical: "x_send".into(),
        input: json!({"chat": "1"}),
        ctx: ctx(),
    });
    roundtrip(&ToolCall {
        call_id: "extra".into(),
        tool_id: None,
        canonical: "x_skip".into(),
        input: json!({}),
        ctx: TurnCtx {
            actor: Actor::unknown(),
            anchor: None,
            run_id: Some("0198-run".into()),
            ..ctx()
        },
    });

    for item in [
        ToolItem::Progress {
            message: None,
            text: "working".into(),
        },
        ToolItem::Result(ToolResult::ok(vec![image()])),
        ToolItem::Result(ToolResult::error("tool timed out")),
        ToolItem::Unknown,
    ] {
        roundtrip(&item);
    }
}

fn decisions<H: Hook>(patch: H::Patch)
where
    H::Patch: Serialize + DeserializeOwned + PartialEq + Debug,
{
    for decision in [
        Decision::Continue,
        Decision::Cancel {
            reason: format!("{} says no", H::POINT),
        },
        Decision::Patch { payload: patch },
        Decision::Unknown,
    ] {
        roundtrip(&decision);
    }
}

#[test]
fn hooks() {
    let before_turn = BeforeTurn {
        ctx: ctx(),
        content: vec![content::Block::text("hi")],
    };
    let before_tool = BeforeTool {
        ctx: ctx(),
        tool_id: Some(ToolId(3)),
        canonical: "x_send".into(),
        input: json!({"chat": 1}),
    };
    let after_tool = AfterTool {
        ctx: ctx(),
        tool_id: None,
        canonical: "x_skip".into(),
        input: json!({}),
        result: ToolResult::ok(vec![image()]),
    };
    roundtrip(&before_turn);
    roundtrip(&before_tool);
    roundtrip(&after_tool);

    decisions::<AfterTool>(ToolResult::error("redacted"));
    // Патч на точке без патчуемого поля не читается: soul считает его отказом хука.
    let patch = encode(&json!({"patch": {"payload": {"chat": 2}}}));
    assert!(rmp_serde::from_slice::<Decision<<BeforeTool as Hook>::Patch>>(&patch).is_err());
    assert!(rmp_serde::from_slice::<Decision<<BeforeTurn as Hook>::Patch>>(&patch).is_err());
    for point in [HookPoint::BeforeTool, HookPoint::BeforeTurn] {
        let decision: Decision<<BeforeTool as Hook>::Patch> =
            rmp_serde::from_slice(&encode(&json!("continue"))).expect("continue decodes");
        assert_eq!(decision, Decision::Continue, "{point}");
    }

    assert_eq!(HookPoint::AfterTool.method(), "hooks.after_tool");
    assert_eq!(encode(&HookPoint::BeforeTool), encode(&"before_tool"));
}

#[test]
fn events_every_topic() {
    let turn = TurnEvent {
        turn_id: "0198-turn".into(),
        chat: ChatId(1),
        surface: "x".into(),
        actor: ctx().actor,
        run_id: None,
        outcome: Some(TurnOutcome::Interrupted),
    };
    let chat = ChatEvent {
        chat: ChatId(2),
        surface: "x".into(),
        external_id: Some("ext".into()),
    };
    let module = ModuleEvent {
        module: "messenger-x".into(),
    };
    let events = [
        Event::TurnStarted(TurnEvent {
            outcome: None,
            ..turn.clone()
        }),
        Event::TurnFinished(turn),
        Event::ToolCalled(ToolCalled {
            turn_id: "0198-turn".into(),
            chat: ChatId(1),
            tool_id: Some(ToolId(4)),
            canonical: "x_send".into(),
            is_error: false,
            duration: HumanDuration(Duration::from_millis(1250)),
        }),
        Event::ChatCreated(chat.clone()),
        Event::ChatDeleted(chat.clone()),
        Event::ChatRestored(chat),
        Event::ChatPurged(ChatPurged {
            ids: vec![ChatId(5), ChatId(6)],
        }),
        Event::Notify(NotifyEvent {
            subject: "core over budget".into(),
            chat_id: ChatId(9),
            source_chat: Some(ChatId(1)),
        }),
        Event::MemoryChanged(MemoryChanged {
            ids: vec![3.into()],
            op: MemoryOp::Supersede,
        }),
        Event::CronFired(CronFired {
            job: "0198-job".into(),
            run_id: "0198-run".into(),
            chat: ChatId(10),
        }),
        Event::SleepPhase(SleepPhase {
            phase: Phase::Groups,
            run_id: "0198-run".into(),
            state: PhaseState::Finished,
        }),
        Event::ModuleConnected(module.clone()),
        Event::ModuleDisconnected(module),
        Event::Unknown,
    ];
    common::assert_every_variant(&events);
    let topics: Vec<Topic> = events.iter().map(Event::topic).collect();
    common::assert_every_variant(&topics);

    for (seq, event) in events.into_iter().enumerate() {
        let topic = event.topic();
        // Тег события и имя топика на проводе совпадают.
        if topic != Topic::Unknown {
            let wire: Value = serde_json::to_value(&event).expect("json");
            assert!(wire.get(topic.as_str()).is_some(), "{topic}: {wire}");
            assert_eq!(encode(&topic), encode(&topic.as_str()));
        }
        let publish = Publish {
            seq: seq as u64,
            at: at(),
            event,
        };
        roundtrip(&publish);

        // `06 §4`: `{topic, seq, at, payload}`, тело без тега.
        let wire: BTreeMap<String, Value> = decode(&encode(&publish));
        assert_eq!(
            wire.keys().map(String::as_str).collect::<Vec<_>>(),
            ["at", "payload", "seq", "topic"]
        );
        if topic != Topic::Unknown {
            assert_eq!(wire["topic"], topic.as_str());
            assert_eq!(
                wire["payload"],
                serde_json::to_value(&publish.event).expect("json")[topic.as_str()]
            );
            let json: Publish =
                serde_json::from_value(serde_json::to_value(&publish).expect("json"))
                    .expect("json publish");
            assert_eq!(json, publish);
        }
    }
}

/// Пир новее: незнакомый топик и лишние поля не рвут соединение.
#[test]
fn publish_tolerant() {
    #[derive(Serialize)]
    struct Newer {
        topic: &'static str,
        seq: u64,
        trace: &'static str,
        at: Timestamp,
        payload: Value,
    }

    let publish: Publish = decode(&encode(&Newer {
        topic: "chat.archived",
        seq: 3,
        trace: "t",
        at: at(),
        payload: json!({"chat": 1, "why": [1, {"x": null}]}),
    }));
    assert_eq!((publish.seq, publish.event), (3, Event::Unknown));

    let purged = Event::ChatPurged(ChatPurged {
        ids: vec![ChatId(5), ChatId(6)],
    });
    let publish: Publish = decode(&encode(&Newer {
        topic: "chat.purged",
        seq: 4,
        trace: "t",
        at: at(),
        payload: json!({"ids": [5, "c6"]}),
    }));
    assert_eq!(publish.event, purged);

    #[derive(Serialize)]
    struct PayloadFirst {
        payload: Value,
        topic: &'static str,
        seq: u64,
        at: Timestamp,
    }
    let payload_first = PayloadFirst {
        payload: json!({"ids": [5, 6]}),
        topic: "chat.purged",
        seq: 1,
        at: at(),
    };
    // В msgpack тег обязан идти первым; JSON-пир может сортировать ключи.
    assert!(rmp_serde::from_slice::<Publish>(&encode(&payload_first)).is_err());
    let json: Publish =
        serde_json::from_str(&serde_json::to_string(&payload_first).expect("json")).expect("json");
    assert_eq!(json.event, purged);

    let no_payload = json!({"topic": "chat.purged", "seq": 1, "at": 1});
    assert!(rmp_serde::from_slice::<Publish>(&encode(&no_payload)).is_err());
    let no_topic = json!({"seq": 1, "at": "2025-06-15T00:00:00Z", "payload": {"ids": []}});
    assert!(serde_json::from_value::<Publish>(no_topic).is_err());
}

/// `11 §7`: целочисленный id на входе — `42`, `"42"` и `"<pfx>42"`; на выходе всегда число.
#[test]
fn int_ids_accept_display_form() {
    fn check<T>(prefixed: &str)
    where
        T: Serialize + DeserializeOwned + PartialEq + Debug + From<i64>,
    {
        let expected = T::from(42);
        for input in [json!(42), json!("42"), json!(prefixed)] {
            assert_eq!(decode::<T>(&encode(&input)), expected, "msgpack {input}");
            let json: T = serde_json::from_value(input.clone())
                .unwrap_or_else(|e| panic!("json {input}: {e}"));
            assert_eq!(json, expected);
        }
        assert_eq!(encode(&expected), encode(&42));
        assert_eq!(serde_json::to_value(&expected).expect("json"), json!(42));

        for bad in [
            json!("x42"),
            json!(""),
            json!("42c"),
            json!(1.5),
            json!(u64::MAX),
        ] {
            assert!(
                rmp_serde::from_slice::<T>(&encode(&bad)).is_err(),
                "msgpack {bad}"
            );
            assert!(
                serde_json::from_value::<T>(bad.clone()).is_err(),
                "json {bad}"
            );
        }
    }

    check::<ChatId>("c42");
    check::<FolderId>("f42");
    check::<MemoryId>("m42");
    check::<ToolId>("tool42");
    assert_eq!(decode::<ChatId>(&encode(&json!("c-7"))), ChatId(-7));

    // Внутри untagged `Selector` и как ключ JSON-карты.
    let args: Chats::get::Args = decode(&encode(&json!({"chat": "c42"})));
    assert_eq!(args.chat, ChatId(42).into());
    let map: BTreeMap<ChatId, u8> =
        serde_json::from_value(json!({"42": 1, "c43": 2})).expect("json keys");
    assert_eq!(map, BTreeMap::from([(ChatId(42), 1), (ChatId(43), 2)]));
}

/// `05 §2.1`: `Effort` читается именем и номером 1..=4, пишется именем; номер —
/// не индекс варианта с нуля.
#[test]
fn effort_names_and_levels() {
    let levels = [
        (1, Effort::Low),
        (2, Effort::Medium),
        (3, Effort::High),
        (4, Effort::Max),
    ];
    for (n, effort) in levels {
        assert_eq!(decode::<Effort>(&encode(&n)), effort, "msgpack {n}");
        let json: Effort = serde_json::from_value(json!(n)).expect("json level");
        assert_eq!(json, effort, "json {n}");
        assert_eq!(effort.as_u8(), n);

        let name = serde_json::to_value(effort).expect("json name");
        assert!(name.is_string(), "{name}");
        assert_eq!(encode(&effort), encode(&name), "written as the name");
        assert_eq!(decode::<Effort>(&encode(&name)), effort, "msgpack {name}");
        let json: Effort = serde_json::from_value(name.clone()).expect("json name");
        assert_eq!(json, effort, "json {name}");
    }

    for bad in [
        json!(0),
        json!(5),
        json!(-1),
        json!(258),
        json!(2.0),
        json!("Medium"),
        json!("2"),
        json!("xhigh"),
        json!(null),
    ] {
        assert!(
            rmp_serde::from_slice::<Effort>(&encode(&bad)).is_err(),
            "msgpack {bad}"
        );
        assert!(
            serde_json::from_value::<Effort>(bad.clone()).is_err(),
            "json {bad}"
        );
    }

    // Под `Patch`/`Option` и в `caps.effort_levels`.
    let update: Surfaces::update::Update = decode(&encode(&json!({"effort": {"change": 2}})));
    assert_eq!(update.effort, Patch::Change(Some(Effort::Medium)));
    let caps: ModelCaps = decode(&encode(&json!({"effort_levels": [1, "high", 4]})));
    assert_eq!(caps.effort_levels, [Effort::Low, Effort::High, Effort::Max]);
}

/// `Patch` поверх msgpack: `nil`, `"keep"` и отсутствие — Keep, `{change: nil}` — очистка.
#[test]
fn patch_over_msgpack() {
    fn check<T>(field: &str, keep: fn(&T) -> bool, cleared: fn(&T) -> bool)
    where
        T: DeserializeOwned + Default + PartialEq + Debug,
    {
        assert_eq!(decode::<T>(&encode(&json!({}))), T::default());
        for form in [json!({field: null}), json!({field: "keep"})] {
            let value: T = decode(&encode(&form));
            assert!(keep(&value), "{form}: {value:?}");
        }
        let value: T = decode(&encode(&json!({field: {"change": null}})));
        assert!(cleared(&value), "{field}: {value:?}");
    }

    check::<Surfaces::update::Update>(
        "rules",
        |u| u.rules.is_keep(),
        |u| u.rules == Patch::Change(None),
    );
    check::<Folders::update::Update>(
        "trusted",
        |u| u.trusted.is_keep(),
        |u| u.trusted == Patch::Change(None),
    );
    check::<Chats::update::Update>(
        "title",
        |u| u.title.is_keep(),
        |u| u.title == Patch::Change(None),
    );

    roundtrip(&Surfaces::update::Update {
        rules: Patch::Change(None),
        name: Patch::Change("n".into()),
        ..Default::default()
    });
    roundtrip(&Surfaces::update::Update {
        rules: Patch::Change(Some("be nice".into())),
        ..Default::default()
    });
    roundtrip(&Folders::update::Update {
        trusted: Patch::Change(None),
        name: Patch::Change("n".into()),
        ..Default::default()
    });
    roundtrip(&Chats::update::Args {
        chat: ChatId(1),
        update: Chats::update::Update {
            title: Patch::Change(None),
            pinned: Patch::Change(true),
            ..Default::default()
        },
    });
}

/// Самые хрупкие формы запросов: `flatten` с `Value` и untagged-селекторы.
#[test]
fn requests_roundtrip() {
    let params = vec![
        Param::Integer(1),
        Param::Float(1.5),
        Param::from("s"),
        Param::Null,
        Param::from(json!([1, {"a": true}])),
        // Байты под `flatten` проходят буфер `Content` и остаются `bin`.
        Param::Bytes(Bytes::from(*b"valid utf-8")),
        Param::Array(vec![Param::Bytes(Bytes::from([0xc1, 0xff]))]),
    ];
    let statement = Statement {
        sql: "SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7".into(),
        params,
    };
    roundtrip(&Sqlite::exec::Args {
        statement: statement.clone(),
    });
    roundtrip(&Sqlite::query::Args {
        statement: statement.clone(),
    });
    roundtrip(&Sqlite::tx::Args {
        statements: vec![statement],
    });
    let bare: Sqlite::query::Args = decode(&encode(&json!({"sql": "SELECT 1"})));
    assert!(bare.statement.params.is_empty());
    let bare: Sqlite::exec::Args = decode(&encode(&json!({"sql": "DELETE FROM t"})));
    assert_eq!(bare.statement.sql, "DELETE FROM t");

    let alias = Selector::Alias {
        surface: "x".into(),
        external_id: "ext-1".into(),
    };
    roundtrip(&Chats::get::Args {
        chat: alias.clone(),
    });
    roundtrip(&Chats::get::Args {
        chat: ChatId(3).into(),
    });

    let name = folder::Selector::Name {
        surface: "x".into(),
        name: "work".into(),
    };
    roundtrip(&Chats::r#move::Args {
        chat: ChatId(1),
        folder: name,
        confirm: true,
    });
    roundtrip(&Chats::r#move::Args {
        chat: ChatId(1),
        folder: FolderId(2).into(),
        confirm: false,
    });

    roundtrip(&Generic::<Chats::get::Err>::Spec(
        errors::chats::NotFound { chat: alias }.into(),
    ));
    roundtrip(&Generic::<Folders::update::Err>::Spec(
        errors::folders::NotFound {
            folder: FolderId(2),
        }
        .into(),
    ));
}

/// `params` у `blobs.put` — поля `Meta` верхним уровнем, без обёртки `meta`.
#[test]
fn blobs_put_params_are_bare_meta() {
    let wire = json!({
        "chat": 1,
        "name": "a.txt",
        "media_type": "text/plain",
        "kind": "file",
        "size": 3,
        "hash": null,
    });
    let meta: Blobs::put::Meta = decode(&encode(&wire));
    assert_eq!(decode::<Value>(&encode(&meta)), wire);
}

/// `Timestamp` рядом с `flatten`/untagged не живёт в msgpack: `blobs.stat` отдаёт `Meta` как есть.
#[test]
fn timestamps_survive() {
    let meta = Meta {
        name: "cat.png".into(),
        media_type: "image/png".into(),
        size: 3,
        hash: "abc".into(),
        kind: blob::Kind::Image,
        expires_at: at(),
    };
    roundtrip::<Blobs::stat::Ok>(&meta.clone().into());
    assert!(
        serde_json::to_value(&meta)
            .expect("json")
            .get("expires_at")
            .is_some(),
        "blobs.stat stays flat"
    );

    roundtrip(&ModelEntry {
        decl: ModelDecl {
            name: "m".into(),
            context_window: 1,
            max_output: 1,
            caps: ModelCaps::default(),
            price: None,
            embed: None,
        },
        provider: "remote".into(),
    });
}

/// Enum'ы, которые пир принимает: незнакомый тег в любой форме — catch-all.
#[test]
fn unknown_tags() {
    #[derive(Serialize)]
    enum Unit {
        #[serde(rename = "brand_new")]
        V,
    }
    #[derive(Serialize)]
    enum Nil {
        #[serde(rename = "brand_new")]
        V(()),
    }
    #[derive(Serialize)]
    enum Map {
        #[serde(rename = "brand_new")]
        V {
            x: u32,
            blob: ByteBuf,
            deep: Vec<BTreeMap<String, Vec<i64>>>,
            at: Timestamp,
        },
    }
    #[derive(Serialize)]
    enum Seq {
        #[serde(rename = "brand_new")]
        V(u32, String, ByteBuf),
    }
    #[derive(Serialize)]
    enum Scalar {
        #[serde(rename = "brand_new")]
        V(i64),
    }

    let forms = [
        encode(&Unit::V),
        encode(&Nil::V(())),
        encode(&Map::V {
            x: 1,
            blob: ByteBuf::from(vec![0xc1, 0xff, 0x00]),
            deep: vec![BTreeMap::from([("k".to_owned(), vec![-1, 300, 70_000])])],
            at: at(),
        }),
        encode(&Seq::V(7, "s".into(), ByteBuf::from(vec![1; 300]))),
        encode(&Scalar::V(-5)),
    ];

    fn check<T: DeserializeOwned + Debug>(forms: &[Vec<u8>], is_catch_all: fn(&T) -> bool) {
        for form in forms {
            let value: T = decode(form);
            assert!(
                is_catch_all(&value),
                "{}: {value:?}",
                std::any::type_name::<T>()
            );
        }
        // JSON-пир тоже терпим.
        let json: T = serde_json::from_value(json!({"brand_new": {"x": [1, {"y": null}]}}))
            .unwrap_or_else(|e| panic!("{}: {e}", std::any::type_name::<T>()));
        assert!(
            is_catch_all(&json),
            "{}: {json:?}",
            std::any::type_name::<T>()
        );
    }

    check::<Frame>(&forms, |v| matches!(v, Frame::Unknown));
    check::<ErrorCode>(&forms, |v| matches!(v, ErrorCode::Unknown));
    check::<GenItem>(&forms, |v| matches!(v, GenItem::Unknown));
    check::<StopReason>(&forms, |v| matches!(v, StopReason::Other));
    check::<ProviderErrorKind>(&forms, |v| matches!(v, ProviderErrorKind::Other));
    check::<FeedEvent>(&forms, |v| matches!(v, FeedEvent::Unknown));
    check::<ItemBody>(&forms, |v| matches!(v, ItemBody::Unknown));
    check::<ItemDelta>(&forms, |v| matches!(v, ItemDelta::Unknown));
    check::<ItemStatus>(&forms, |v| matches!(v, ItemStatus::Unknown));
    check::<PendingKind>(&forms, |v| matches!(v, PendingKind::Unknown));
    check::<Button>(&forms, |v| matches!(v, Button::Unknown));
    check::<PendingOutcome>(&forms, |v| matches!(v, PendingOutcome::Unknown));
    check::<TaskStatus>(&forms, |v| matches!(v, TaskStatus::Unknown));
    check::<ToolContent>(&forms, |v| matches!(v, ToolContent::Unknown));
    check::<ToolItem>(&forms, |v| matches!(v, ToolItem::Unknown));
    check::<Topic>(&forms, |v| matches!(v, Topic::Unknown));
    check::<Event>(&forms, |v| matches!(v, Event::Unknown));
    check::<TurnOutcome>(&forms, |v| matches!(v, TurnOutcome::Unknown));
    check::<MemoryOp>(&forms, |v| matches!(v, MemoryOp::Unknown));
    check::<PhaseState>(&forms, |v| matches!(v, PhaseState::Unknown));
    check::<Phase>(&forms, |v| matches!(v, Phase::Unknown));
    check::<HookPoint>(&forms, |v| matches!(v, HookPoint::Unknown));
    check::<Decision<Value>>(&forms, |v| matches!(v, Decision::Unknown));
    check::<FailMode>(&forms, |v| matches!(v, FailMode::Open));
    check::<Need>(&forms, |v| matches!(v, Need::Unknown));
    check::<ResultForm>(&forms, |v| matches!(v, ResultForm::Windowed));
    check::<ActorKind>(&forms, |v| matches!(v, ActorKind::Unknown));
}

/// Payload незнакомого варианта пропущен ровно: соседи разбираются дальше.
#[test]
fn unknown_payload_is_skipped_exactly() {
    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    enum NewerContent {
        Text {
            text: String,
        },
        Video {
            frames: Vec<ByteBuf>,
            meta: BTreeMap<String, Vec<u32>>,
        },
        Blob {
            id: String,
        },
    }

    let contents: Vec<ToolContent> = decode(&encode(&vec![
        NewerContent::Text { text: "a".into() },
        NewerContent::Video {
            frames: vec![ByteBuf::from(vec![1; 5]), ByteBuf::from(vec![2; 70_000])],
            meta: BTreeMap::from([("fps".to_owned(), vec![24, 30])]),
        },
        NewerContent::Blob { id: "b".into() },
    ]));
    assert_eq!(
        contents,
        [
            ToolContent::text("a"),
            ToolContent::Unknown,
            ToolContent::Blob { id: "b".into() },
        ]
    );

    #[derive(Serialize)]
    struct NewerTurnItem {
        seq: i64,
        event: BTreeMap<&'static str, Value>,
        replayed: bool,
        extra: &'static str,
    }

    let items: Vec<TurnItem> = decode(&encode(&vec![
        NewerTurnItem {
            seq: 1,
            event: BTreeMap::from([("turn_paused", json!({"turn": "t", "why": [1, 2]}))]),
            replayed: true,
            extra: "ignored",
        },
        NewerTurnItem {
            seq: 2,
            event: BTreeMap::from([(
                "turn_done",
                json!({"turn": "t", "outcome": "failed", "error": "boom"}),
            )]),
            replayed: false,
            extra: "ignored",
        },
    ]));
    assert_eq!(items[0].event, FeedEvent::Unknown);
    assert!(items[0].replayed);
    assert_eq!(
        items[1].event,
        FeedEvent::TurnDone {
            turn: "t".into(),
            outcome: TurnOutcome::Failed,
            error: Some("boom".into()),
        }
    );
}

/// Сломанная карта варианта — ошибка, не тихий `Unknown`.
#[test]
fn malformed_variant_map_fails() {
    let two_keys = encode(&json!({"text_delta": {"text": "a"}, "stop": {"reason": "other"}}));
    assert!(rmp_serde::from_slice::<GenItem>(&two_keys).is_err());

    let empty = encode(&json!({}));
    assert!(rmp_serde::from_slice::<GenItem>(&empty).is_err());

    let bad_payload = encode(&json!({"text_delta": {"text": 5}}));
    assert!(rmp_serde::from_slice::<GenItem>(&bad_payload).is_err());
}

/// BLOB — `bin` и назад `Blob`, даже если его байты — валидный UTF-8.
#[test]
fn sqlite_cells_keep_their_storage_class() {
    let ok = Sqlite::query::Ok {
        columns: vec!["n".into(), "i".into(), "r".into(), "t".into(), "b".into()],
        rows: vec![vec![
            Cell::Null,
            Cell::Integer(-7),
            Cell::Real(1.5),
            Cell::Text("abc".into()),
            Cell::Blob(Bytes::from(*b"abc")),
        ]],
    };
    roundtrip(&ok);

    let blob = encode(&Cell::Blob(Bytes::from(*b"abc")));
    assert_eq!(blob, [0xc4, 3, b'a', b'b', b'c']);
    assert!(rmp_serde::from_slice::<Cell>(&encode(&u64::MAX)).is_err());
}

/// Произвольные строки, байты, числа и время: форма провода не зависит от
/// выбранных образцов (`06 §12`).
mod props {
    use std::num::NonZeroU64;

    use proptest::{collection::vec, option, prelude::*};

    use super::*;

    fn text() -> impl Strategy<Value = String> {
        "\\PC{0,24}"
    }

    fn raw() -> impl Strategy<Value = Bytes> {
        prop_oneof![vec(any::<u8>(), 0..64), vec(any::<u8>(), 250..300)].prop_map(Bytes::from)
    }

    fn cost() -> impl Strategy<Value = Option<f64>> {
        option::of(-1.0e12..1.0e12_f64)
    }

    fn moment() -> impl Strategy<Value = Option<Timestamp>> {
        option::of(any::<NonZeroU64>().prop_map(Timestamp::from_nanos))
    }

    fn gen_item() -> impl Strategy<Value = GenItem> {
        prop_oneof![
            text().prop_map(|text| GenItem::TextDelta { text }),
            text().prop_map(|text| GenItem::ThinkingDelta { text }),
            (raw(), text()).prop_map(|(signature, model)| GenItem::ThinkingBlock {
                signature,
                model: model.into(),
            }),
            (raw(), text()).prop_map(|(data, model)| GenItem::RedactedThinking {
                data,
                model: model.into(),
            }),
            (text(), text(), any::<i64>()).prop_map(|(id, name, n)| GenItem::ToolUse {
                input: json!({"name": name, "n": n, "list": [n, null]}),
                id,
                name,
            }),
            (text(), raw()).prop_map(|(media_type, data)| GenItem::Image { media_type, data }),
            (
                any::<[u64; 6]>(),
                any::<(u32, u32)>(),
                cost(),
                option::of((option::of(any::<u64>()), option::of(any::<u64>()), moment())),
            )
                .prop_map(|(n, (applied, cleared), cost, limits)| {
                    GenItem::Usage(Usage {
                        input: n[0],
                        cache_read: n[1],
                        cache_write_short: n[2],
                        cache_write_long: n[3],
                        output: n[4],
                        reasoning: n[5],
                        context_edits: ContextEdits {
                            applied,
                            cleared_input_tokens: cleared,
                        },
                        cost,
                        rate_limits: limits.map(|(requests, tokens, reset_at)| RateLimits {
                            requests_remaining: requests,
                            tokens_remaining: tokens,
                            reset_at,
                        }),
                    })
                }),
            option::of(text()).prop_map(|details| GenItem::Stop(Stop {
                reason: StopReason::MaxTokens,
                details,
            })),
            Just(GenItem::Unknown),
        ]
    }

    fn frame() -> impl Strategy<Value = Frame> {
        prop_oneof![
            (any::<u64>(), text(), raw()).prop_map(|(id, method, params)| {
                Frame::Request(Request { id, method, params })
            }),
            (any::<u64>(), any::<u64>(), raw())
                .prop_map(|(id, seq, item)| Frame::StreamItem(StreamItem { id, seq, item })),
            (any::<u64>(), raw())
                .prop_map(|(id, result)| Frame::Response(Response::ok(id, result))),
            (
                any::<u64>(),
                text(),
                option::of(raw()),
                option::of(any::<u32>())
            )
                .prop_map(|(id, message, data, retry)| {
                    Frame::StreamEnd(StreamEnd {
                        id,
                        error: Some(RpcError {
                            code: ErrorCode::Quota,
                            message,
                            data,
                            retry_after: retry.map(|s| secs(s.into())),
                        }),
                    })
                }),
            (text(), raw()).prop_map(|(method, params)| Frame::Notify(Notify { method, params })),
        ]
    }

    fn feed_event() -> impl Strategy<Value = FeedEvent> {
        let body = prop_oneof![
            (
                text(),
                option::of(text()),
                any::<bool>(),
                option::of(text())
            )
                .prop_map(|(text, sender, master, external_id)| {
                    ItemBody::UserMessage {
                        text,
                        sender: sender.map(Into::into),
                        master,
                        external_id,
                    }
                }),
            (text(), cost()).prop_map(|(text, cost)| ItemBody::AgentMessage { text, cost }),
            (text(), text(), text(), raw(), option::of(text())).prop_map(
                |(call_id, name, media_type, data, progress)| ItemBody::ToolCall {
                    input: json!({"name": name}),
                    call_id,
                    name,
                    agent: None,
                    result: Some(ToolCallResult {
                        output: vec![ToolContent::Image { media_type, data }],
                        is_error: false,
                    }),
                    progress,
                }
            ),
        ];
        prop_oneof![
            (
                text(),
                option::of(text()),
                any::<i64>(),
                body,
                option::of(text())
            )
                .prop_map(|(id, turn, seq, body, replaces)| FeedEvent::ItemCompleted {
                    item: Item {
                        id,
                        turn: turn.map(Into::into),
                        seq: EventId(seq),
                        status: ItemStatus::Completed,
                        body,
                    },
                    replaces,
                }),
            (text(), text()).prop_map(|(id, text)| FeedEvent::ItemDelta {
                id,
                delta: ItemDelta::Text { text },
            }),
            (text(), any::<u32>(), text(), any::<[u64; 4]>(), cost()).prop_map(
                |(turn, step, model, n, cost)| FeedEvent::Usage(StepUsage {
                    turn: turn.into(),
                    step,
                    model: model.into(),
                    input: n[0],
                    cache_read: n[1],
                    cache_write: n[2],
                    output: n[3],
                    cost,
                })
            ),
            (text(), option::of(text())).prop_map(|(turn, error)| FeedEvent::TurnDone {
                turn: turn.into(),
                outcome: TurnOutcome::Completed,
                error,
            }),
        ]
    }

    proptest! {
        #[test]
        fn gen_items_roundtrip(item in gen_item()) {
            prop_assert_eq!(decode::<GenItem>(&encode(&item)), item);
        }

        #[test]
        fn frames_roundtrip(frame in frame()) {
            prop_assert_eq!(decode::<Frame>(&encode(&frame)), frame);
        }

        /// Кадр несёт payload вторым шагом: `GenItem` внутри `stream_item` цел.
        #[test]
        fn stream_item_carries_gen_item(id in any::<u64>(), seq in any::<u64>(), item in gen_item()) {
            let frame = Frame::StreamItem(StreamItem { id, seq, item: encode(&item).into() });
            let Frame::StreamItem(back) = decode::<Frame>(&encode(&frame)) else {
                panic!("stream_item expected");
            };
            prop_assert_eq!((back.id, back.seq), (id, seq));
            prop_assert_eq!(decode::<GenItem>(&back.item), item);
        }

        #[test]
        fn turn_items_roundtrip(
            seq in any::<i64>(),
            sub in any::<u32>(),
            replayed in any::<bool>(),
            event in feed_event(),
        ) {
            let item = TurnItem { seq: EventId(seq), sub, replayed, event };
            prop_assert_eq!(decode::<TurnItem>(&encode(&item)), item);
        }

        #[test]
        fn params_roundtrip(raw in raw(), s in text(), i in any::<i64>(), u in any::<u64>()) {
            let statement = Statement {
                sql: s.clone(),
                params: vec![
                    Param::Bytes(raw.clone()),
                    Param::String(s.clone()),
                    Param::Integer(i),
                    Param::from(u),
                    Param::Map(vec![(Param::String(s), Param::Array(vec![Param::Bytes(raw)]))]),
                ],
            };
            let args = Sqlite::exec::Args { statement };
            prop_assert_eq!(decode::<Sqlite::exec::Args>(&encode(&args)), args);
        }
    }
}
