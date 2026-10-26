//! Тело `POST /chat/completions` по нормализованному запросу: снимки и отказы.

use serde_json::{Value, json};

use eva_protocol::types::{
    Bytes, Effort, ModelId, Thinking,
    generate::{
        Block, CacheMode, GenerateRequest, Message, OutputFormat, Position, ProviderErrorKind,
        Purpose, ResultBlock, SystemKind, SystemSection, ToolSpec,
    },
    model::ModelDecl,
};
use eva_provider_openai::{Config, Dialect, declare, render};

const SONNET: &str = "anthropic/claude-sonnet-4.5";
const MINI: &str = "openai/gpt-5-mini";
const IMAGE: &str = "google/gemini-2.5-flash-image";
/// Сигнатура PNG: содержимое не важно, важен data URL.
const PNG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

fn model(name: &str) -> ModelDecl {
    let listing: Value =
        serde_json::from_str(include_str!("fixtures/openrouter_models.json")).unwrap();
    declare(Some(&listing), &Config::default())
        .into_iter()
        .find(|m| m.name.as_str() == name)
        .unwrap()
}

fn request(model: &str, messages: Vec<Message>) -> GenerateRequest {
    GenerateRequest {
        model: ModelId::from(model),
        system: vec![
            SystemSection {
                name: "persona".into(),
                text: "[persona]\nYou are Eva.".into(),
            },
            SystemSection {
                name: "chat".into(),
                text: "[chat]\nParticipants: master.".into(),
            },
        ],
        tools: Vec::new(),
        messages,
        breakpoints: Vec::new(),
        cache: CacheMode::Off,
        effort: None,
        thinking: None,
        max_tokens: 4096,
        format: None,
        purpose: Purpose::Turn,
    }
}

fn text(text: &str) -> Block {
    Block::Text { text: text.into() }
}

fn user(blocks: Vec<Block>) -> Message {
    Message::User { blocks }
}

fn assistant(blocks: Vec<Block>) -> Message {
    Message::Assistant { blocks }
}

fn body(req: &GenerateRequest, model_name: &str) -> Value {
    render(req, &model(model_name), Dialect::Openrouter).unwrap()
}

#[test]
fn plain_chat() {
    let req = request(MINI, vec![user(vec![text("hello")])]);
    insta::assert_json_snapshot!("plain_chat", body(&req, MINI));

    let local = ModelDecl {
        name: ModelId::from("qwen3"),
        ..model("meta-llama/llama-3.3-70b-instruct")
    };
    let req = GenerateRequest {
        model: local.name.clone(),
        ..req
    };
    insta::assert_json_snapshot!(
        "plain_chat_plain_dialect",
        render(&req, &local, Dialect::Plain).unwrap()
    );
}

#[test]
fn tools_skip_deferred_and_fold_one_of() {
    let mut req = request(MINI, vec![user(vec![text("list my notes")])]);
    req.tools = vec![
        ToolSpec {
            name: "notes_list".into(),
            description: "List notes.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "filter": {"oneOf": [{"type": "string"}, {"type": "null"}]}
                }
            }),
            deferred: false,
        },
        ToolSpec {
            name: "cron_create".into(),
            description: "Deferred tool, loaded on demand.".into(),
            input_schema: json!({"type": "object"}),
            deferred: true,
        },
    ];
    insta::assert_json_snapshot!("tools", body(&req, MINI));
}

#[test]
fn tool_round_trip_with_error() {
    let req = request(
        MINI,
        vec![
            user(vec![text("read both files")]),
            assistant(vec![
                text("Reading."),
                Block::ToolUse {
                    id: "call_1".into(),
                    name: "fs_read".into(),
                    input: json!({"path": "a.txt"}),
                },
                Block::ToolUse {
                    id: "call_2".into(),
                    name: "fs_read".into(),
                    input: Value::String("{\"path\": \"b.t".into()),
                },
            ]),
            user(vec![
                Block::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: vec![ResultBlock::Text {
                        text: "[t1]\nalpha".into(),
                    }],
                    is_error: false,
                },
                Block::ToolResult {
                    tool_use_id: "call_2".into(),
                    content: vec![ResultBlock::Text {
                        text: "[t2]\ninvalid input".into(),
                    }],
                    is_error: true,
                },
                text("[soul] note updated"),
            ]),
        ],
    );
    insta::assert_json_snapshot!("tool_round_trip", body(&req, MINI)["messages"]);
}

#[test]
fn thinking_replay_keeps_reasoning_details_verbatim() {
    let signed = json!({
        "type": "reasoning.text",
        "text": "Need the path.",
        "signature": "EqQBCkYIBxgCKkDsig==",
        "format": "anthropic-claude-v1",
        "index": 0,
    });
    let encrypted = json!({
        "type": "reasoning.encrypted",
        "data": "gAAAAABo-opaque",
        "format": "anthropic-claude-v1",
        "index": 1,
    });
    let thinking = |detail: &Value, model: &str| Block::Thinking {
        signature: Bytes(serde_json::to_vec(detail).unwrap()),
        model: ModelId::from(model),
        text: Some("Need the path.".into()),
    };
    let req = request(
        SONNET,
        vec![
            user(vec![text("read it")]),
            assistant(vec![
                thinking(&signed, SONNET),
                Block::RedactedThinking {
                    data: Bytes(serde_json::to_vec(&encrypted).unwrap()),
                    model: ModelId::from(SONNET),
                },
                thinking(&json!({"type": "reasoning.text", "text": "foreign"}), MINI),
                Block::Thinking {
                    signature: Bytes(b"raw-vendor-signature".to_vec()),
                    model: ModelId::from(SONNET),
                    text: None,
                },
                Block::ToolUse {
                    id: "toolu_1".into(),
                    name: "fs_read".into(),
                    input: json!({"path": "a"}),
                },
            ]),
            user(vec![Block::ToolResult {
                tool_use_id: "toolu_1".into(),
                content: vec![ResultBlock::Text { text: "ok".into() }],
                is_error: false,
            }]),
            // Одни мысли без ответа и вызовов — сообщения нет.
            assistant(vec![thinking(&signed, SONNET)]),
            user(vec![text("go on")]),
        ],
    );
    let messages = body(&req, SONNET)["messages"].clone();
    insta::assert_json_snapshot!("thinking_replay", messages);
    assert_eq!(messages[2]["reasoning_details"], json!([signed, encrypted]));

    let plain = render(&req, &model(SONNET), Dialect::Plain).unwrap();
    assert!(plain["messages"][2].get("reasoning_details").is_none());
}

fn conversation(model: &str, cache: CacheMode) -> GenerateRequest {
    let mut req = request(
        model,
        vec![
            user(vec![text("first")]),
            assistant(vec![text("answer")]),
            user(vec![text("second"), text("[soul] reminder")]),
        ],
    );
    req.cache = cache;
    req.breakpoints = vec![
        Position::System,
        Position::Message { index: 0 },
        Position::Message { index: 2 },
    ];
    req
}

#[test]
fn breakpoints_follow_cache_ttl_on_explicit_cache_models() {
    insta::assert_json_snapshot!(
        "breakpoints_short",
        body(&conversation(SONNET, CacheMode::Short), SONNET)["messages"]
    );
    insta::assert_json_snapshot!(
        "breakpoints_long",
        body(&conversation(SONNET, CacheMode::Long), SONNET)["messages"]
    );

    let off = body(&conversation(SONNET, CacheMode::Off), SONNET);
    let automatic = body(&conversation(MINI, CacheMode::Long), MINI);
    for body in [&off, &automatic] {
        assert!(!body.to_string().contains("cache_control"), "{body}");
    }
    insta::assert_json_snapshot!("breakpoints_automatic_cache", automatic["messages"]);

    let mut too_many = conversation(SONNET, CacheMode::Short);
    too_many.breakpoints.extend([
        Position::Message { index: 1 },
        Position::Message { index: 3 },
    ]);
    let error = render(&too_many, &model(SONNET), Dialect::Openrouter).unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Invalid);
}

#[test]
fn breakpoint_on_a_tool_calls_only_message_is_dropped() {
    let mut req = request(
        SONNET,
        vec![
            user(vec![text("run")]),
            assistant(vec![Block::ToolUse {
                id: "t1".into(),
                name: "shell".into(),
                input: json!({}),
            }]),
            user(vec![Block::ToolResult {
                tool_use_id: "t1".into(),
                content: vec![ResultBlock::Text { text: "out".into() }],
                is_error: false,
            }]),
        ],
    );
    req.cache = CacheMode::Short;
    req.breakpoints = vec![
        Position::Message { index: 1 },
        Position::Message { index: 2 },
    ];
    let messages = body(&req, SONNET)["messages"].clone();
    assert_eq!(messages[2]["content"], Value::Null);
    assert!(messages[2].get("cache_control").is_none());
    assert_eq!(
        messages[3]["content"],
        json!([{"type": "text", "text": "out", "cache_control": {"type": "ephemeral"}}])
    );
}

#[test]
fn effort_and_thinking() {
    let listed = ModelDecl {
        caps: eva_protocol::types::model::ModelCaps {
            effort_levels: vec![Effort::Low, Effort::Medium, Effort::High, Effort::Max],
            ..model(MINI).caps
        },
        ..model(MINI)
    };
    let reasoning = |effort, thinking, model: &ModelDecl, dialect| {
        let mut req = request(model.name.as_str(), vec![user(vec![text("hi")])]);
        req.effort = effort;
        req.thinking = thinking;
        let body = render(&req, model, dialect).unwrap();
        json!({
            "reasoning": body.get("reasoning"),
            "reasoning_effort": body.get("reasoning_effort"),
        })
    };
    insta::assert_json_snapshot!(
        "effort",
        json!({
            "max_unlisted": reasoning(Some(Effort::Max), None, &model(SONNET), Dialect::Openrouter),
            "max_listed": reasoning(Some(Effort::Max), None, &listed, Dialect::Openrouter),
            "low_adaptive": reasoning(Some(Effort::Low), Some(Thinking::Adaptive), &model(SONNET), Dialect::Openrouter),
            "adaptive_without_effort": reasoning(None, Some(Thinking::Adaptive), &model(SONNET), Dialect::Openrouter),
            "off_wins": reasoning(Some(Effort::High), Some(Thinking::Off), &model(SONNET), Dialect::Openrouter),
            "no_reasoning_model": reasoning(Some(Effort::High), Some(Thinking::Off), &model("meta-llama/llama-3.3-70b-instruct"), Dialect::Openrouter),
            "plain_max_listed": reasoning(Some(Effort::Max), None, &listed, Dialect::Plain),
            "plain_off": reasoning(Some(Effort::High), Some(Thinking::Off), &listed, Dialect::Plain),
        })
    );
}

#[test]
fn structured_output() {
    let mut req = request(MINI, vec![user(vec![text("plan")])]);
    req.format = Some(OutputFormat {
        schema: json!({
            "type": "object",
            "properties": {"ops": {"type": "array", "items": {"oneOf": [{"type": "string"}, {"type": "integer"}]}}},
            "required": ["ops"]
        }),
        strict: true,
    });
    insta::assert_json_snapshot!("structured_output", body(&req, MINI)["response_format"]);

    let unsupported = body(&req, "meta-llama/llama-3.3-70b-instruct");
    assert!(unsupported.get("response_format").is_none());
}

#[test]
fn images_in_and_out() {
    let req = request(
        IMAGE,
        vec![
            user(vec![
                text("what is this?"),
                Block::Image {
                    media_type: "image/png".into(),
                    data: Bytes(PNG.to_vec()),
                },
            ]),
            assistant(vec![
                text("A cat. Here is another:"),
                Block::Image {
                    media_type: "image/png".into(),
                    data: Bytes(PNG.to_vec()),
                },
                Block::ToolUse {
                    id: "c1".into(),
                    name: "screenshot".into(),
                    input: json!({}),
                },
            ]),
            user(vec![Block::ToolResult {
                tool_use_id: "c1".into(),
                content: vec![
                    ResultBlock::Text {
                        text: "[t1]\nscreen".into(),
                    },
                    ResultBlock::Image {
                        media_type: "image/jpeg".into(),
                        data: Bytes(vec![0xff, 0xd8, 0xff]),
                    },
                ],
                is_error: false,
            }]),
        ],
    );
    let body = body(&req, IMAGE);
    insta::assert_json_snapshot!("images", body);
    assert_eq!(body["modalities"], json!(["image", "text"]));

    let sonnet = render(
        &GenerateRequest {
            model: ModelId::from(SONNET),
            ..req
        },
        &model(SONNET),
        Dialect::Openrouter,
    )
    .unwrap();
    let assistant = &sonnet["messages"][2]["content"];
    assert_eq!(
        assistant, "A cat. Here is another:",
        "no image_out, no image in assistant"
    );
    assert!(sonnet.get("modalities").is_none());
}

#[test]
fn system_messages_need_the_capability() {
    let messages = vec![
        user(vec![text("hi")]),
        Message::System {
            kind: SystemKind::NoteUpdated,
            blocks: vec![text("[notes] saved: plan")],
            clear_at: None,
            output_config: None,
            tool_changes: None,
        },
    ];
    let req = request(MINI, messages);
    let error = render(&req, &model(MINI), Dialect::Openrouter).unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Invalid);

    let mut native = model(MINI);
    native.caps.system_in_messages = true;
    let body = render(&req, &native, Dialect::Openrouter).unwrap();
    assert_eq!(
        body["messages"][2],
        json!({"role": "system", "content": "[notes] saved: plan"})
    );

    let mut clear = req.clone();
    if let Message::System { clear_at, .. } = &mut clear.messages[1] {
        *clear_at = Some(eva_protocol::types::generate::ClearAt::NextUserMessage);
    }
    let error = render(&clear, &native, Dialect::Openrouter).unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Invalid);
}

#[test]
fn keep_alive_is_refused() {
    let mut req = request(SONNET, vec![user(vec![text("hi")])]);
    req.purpose = Purpose::KeepAlive;
    req.max_tokens = 0;
    let error = render(&req, &model(SONNET), Dialect::Openrouter).unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Invalid);
}
