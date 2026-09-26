//! msgpack-провод (`to_vec_named`): roundtrip wire-типов и терпимость к
//! вариантам пира новее (`06 §1, §12`).

use std::{collections::BTreeMap, fmt::Debug, time::Duration};

use eva::time::{HumanDuration, Timestamp};
use serde::{Serialize, de::DeserializeOwned};
use serde_bytes::ByteBuf;
use serde_json::{Value, json};

use eva_protocol::{
    errors::{self, Generic},
    requests::{Patch, blobs, chats, folders, sqlite, surfaces},
    types::{
        Bytes, ChatId, Effort, EventId, FolderId, MemoryId, Thinking, ToolId,
        blob::{self, Meta},
        content, folder,
        generate::{
            Block, CacheMode, ClearAt, ContextEdits, GenItem, GenerateRequest, Message,
            OutputConfig, OutputFormat, Position, ProviderErrorKind, Purpose, RateLimits,
            ResultBlock, Stop, StopReason, SystemKind, SystemSection, ToolChanges, ToolSpec, Usage,
        },
        model::{EmbedDecl, ModelCaps, ModelDecl, ModelEntry, Price, ProviderDecl},
        module::TokenKind,
        sleep::Phase,
        tool::{GroupDecl, Need, ResultForm, ToolContent, ToolDecl},
        turn::{
            Actor, ActorKind, Decision as Button, FeedEvent, Item, ItemBody, ItemDelta, ItemStatus,
            PendingKind, PendingOutcome, PlanTask, StepUsage, TaskStatus, ToolCallResult, TurnCtx,
            TurnItem,
        },
    },
    wire::{
        Capabilities, ErrorCode, Frame, Hello, RpcError, Welcome,
        capabilities::{EventsCap, FailMode, HooksCap, ProviderCap, ToolsCap},
        events::{
            ChatEvent, ChatPurged, CronFired, Event, MemoryChanged, MemoryOp, ModuleEvent,
            NotifyEvent, PhaseState, Publish, SleepPhase, ToolCalled, Topic, TurnEvent,
            TurnOutcome,
        },
        frame::{Cancel, Notify, Request, Response, StreamEnd, StreamHeartbeat, StreamItem},
        handshake::{Ping, ProtoVersion, Resume, Warning},
        hooks::{AfterTool, BeforeSend, BeforeTool, BeforeTurn, Decision, Hook, HookPoint},
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
        idempotent: true,
    }
}

#[test]
fn frame_every_variant() {
    let params = chats::delete::Args {
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
        Frame::Response(Response::ok(1, encode(&chats::delete::Ok {}).into())),
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
    for frame in &frames {
        roundtrip(frame);
    }

    // Второй шаг разбора: payload кадра — msgpack `Args`.
    let Frame::Request(request) = decode(&encode(&frames[0])) else {
        panic!("request expected");
    };
    assert_eq!(decode::<chats::delete::Args>(&request.params), params);
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
            points: vec![HookPoint::BeforeSend, HookPoint::AfterTool],
            timeout: Some(HumanDuration(Duration::from_millis(1500))),
            fail_mode: FailMode::Closed,
        }),
    };
    assert_eq!(
        capabilities.names(),
        ["provider", "tools", "events", "hooks"]
    );

    let hello = Hello {
        module: "messenger-x".into(),
        version: "1.2.3".into(),
        proto_version: ProtoVersion::CURRENT,
        token: "secret".into(),
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
    roundtrip(&Capabilities::default());

    let welcome = Welcome {
        session_id: "0198-session".into(),
        kernel_version: "0.1.0".into(),
        proto_version: ProtoVersion { major: 0, minor: 1 },
        kind: TokenKind::Service,
        services: vec!["chats".into(), "turns".into()],
        tool_ids: BTreeMap::from([("x.send".into(), ToolId(17))]),
        models_accepted: vec!["vendor/model".into()],
        warnings: vec![Warning {
            code: ErrorCode::Forbidden,
            subject: "provider".into(),
            message: "module is not named in llm.providers".into(),
        }],
    };
    roundtrip(&welcome);

    // Пустые секции `welcome` опциональны на проводе.
    let minimal: Welcome = decode(&encode(&json!({
        "session_id": "s",
        "kernel_version": "0.1.0",
        "proto_version": "0.1",
        "kind": "client",
    })));
    assert!(minimal.tool_ids.is_empty() && minimal.warnings.is_empty());

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
                    text: "[kernel] 12:00".into(),
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
        ItemBody::Compaction {
            summary_ref: 3.into(),
        },
        ItemBody::Reminder {
            text: "[kernel] 12:00".into(),
        },
        ItemBody::Plan {
            tasks: vec![task.clone()],
        },
        ItemBody::Unknown,
    ];

    let mut events: Vec<FeedEvent> = bodies
        .into_iter()
        .map(|body| FeedEvent::ItemCompleted {
            item: item(ItemStatus::Completed, body),
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
            delta: ItemDelta::Progress { text: "1/3".into() },
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
            message_ids: vec![EventId(101), EventId(102)],
        },
        FeedEvent::TurnError {
            turn: "0198-turn".into(),
            message: "provider failed".into(),
        },
        FeedEvent::Unknown,
    ]);

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
    let before_send = BeforeSend {
        ctx: ctx(),
        text: "reply".into(),
        attachments: vec![content::Block::File {
            blob: "0198-blob".into(),
        }],
    };
    roundtrip(&before_turn);
    roundtrip(&before_tool);
    roundtrip(&after_tool);
    roundtrip(&before_send);

    decisions::<BeforeTurn>(vec![content::Block::text("patched")]);
    decisions::<BeforeTool>(json!({"chat": 2}));
    decisions::<AfterTool>(ToolResult::error("redacted"));
    decisions::<BeforeSend>("patched".to_owned());

    assert_eq!(HookPoint::AfterTool.method(), "hooks.after_tool");
    assert_eq!(encode(&HookPoint::BeforeSend), encode(&"before_send"));
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
    let args: chats::get::Args = decode(&encode(&json!({"chat": "c42"})));
    assert_eq!(args.chat, ChatId(42).into());
    let map: BTreeMap<ChatId, u8> =
        serde_json::from_value(json!({"42": 1, "c43": 2})).expect("json keys");
    assert_eq!(map, BTreeMap::from([(ChatId(42), 1), (ChatId(43), 2)]));
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

    check::<surfaces::update::Update>(
        "rules",
        |u| u.rules.is_keep(),
        |u| u.rules == Patch::Change(None),
    );
    check::<folders::update::Update>(
        "trusted",
        |u| u.trusted.is_keep(),
        |u| u.trusted == Patch::Change(None),
    );
    check::<chats::update::Update>(
        "title",
        |u| u.title.is_keep(),
        |u| u.title == Patch::Change(None),
    );

    roundtrip(&surfaces::update::Update {
        rules: Patch::Change(None),
        name: Patch::Change("n".into()),
        ..Default::default()
    });
    roundtrip(&surfaces::update::Update {
        rules: Patch::Change(Some("be nice".into())),
        ..Default::default()
    });
    roundtrip(&folders::update::Update {
        trusted: Patch::Change(None),
        name: Patch::Change("n".into()),
        ..Default::default()
    });
    roundtrip(&chats::update::Args {
        chat: ChatId(1),
        update: chats::update::Update {
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
        json!(1),
        json!(1.5),
        json!("s"),
        Value::Null,
        json!([1, {"a": true}]),
    ];
    let statement = sqlite::Statement {
        sql: "SELECT ?1, ?2, ?3, ?4, ?5".into(),
        params,
    };
    roundtrip(&sqlite::exec::Args {
        statement: statement.clone(),
    });
    roundtrip(&sqlite::query::Args {
        statement: statement.clone(),
    });
    roundtrip(&sqlite::tx::Args {
        statements: vec![statement],
    });
    let bare: sqlite::query::Args = decode(&encode(&json!({"sql": "SELECT 1"})));
    assert!(bare.statement.params.is_empty());
    let bare: sqlite::exec::Args = decode(&encode(&json!({"sql": "DELETE FROM t"})));
    assert_eq!(bare.statement.sql, "DELETE FROM t");

    let alias = chats::Selector::Alias {
        surface: "x".into(),
        external_id: "ext-1".into(),
    };
    roundtrip(&chats::get::Args {
        chat: alias.clone(),
    });
    roundtrip(&chats::get::Args {
        chat: ChatId(3).into(),
    });

    let name = folder::Selector::Name {
        surface: "x".into(),
        name: "work".into(),
    };
    roundtrip(&chats::r#move::Args {
        chat: ChatId(1),
        folder: name,
        confirm: true,
    });
    roundtrip(&chats::r#move::Args {
        chat: ChatId(1),
        folder: FolderId(2).into(),
        confirm: false,
    });

    roundtrip(&Generic::<chats::get::Err>::Spec(
        errors::chats::NotFound { chat: alias }.into(),
    ));
    roundtrip(&Generic::<folders::update::Err>::Spec(
        errors::folders::NotFound {
            folder: FolderId(2),
        }
        .into(),
    ));
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
    roundtrip::<blobs::stat::Ok>(&meta);
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
            event: BTreeMap::from([("turn_error", json!({"turn": "t", "message": "boom"}))]),
            replayed: false,
            extra: "ignored",
        },
    ]));
    assert_eq!(items[0].event, FeedEvent::Unknown);
    assert!(items[0].replayed);
    assert_eq!(
        items[1].event,
        FeedEvent::TurnError {
            turn: "t".into(),
            message: "boom".into(),
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
