//! Сценарии, запись запросов и громкие отказы на невалидной истории.

use futures::{StreamExt, channel::mpsc};
use serde_json::json;

use eva_core::{GenSink, Provider};
use eva_protocol::types::{
    Bytes, ModelId,
    generate::{
        Block, CacheMode, EmbedRequest, GenItem, GenerateRequest, Message, Position, ProviderError,
        ProviderErrorKind, Purpose, ResultBlock, Stop, StopReason, SystemKind, SystemSection,
        ToolSpec, Usage,
    },
};
use eva_provider_mock::{CHAT_MODEL, Config, EMBED_MODEL, Exhausted, MockProvider, Script};

fn request(messages: Vec<Message>) -> GenerateRequest {
    GenerateRequest {
        model: ModelId::from(CHAT_MODEL),
        system: vec![SystemSection {
            name: "persona".into(),
            text: "You are Eva.".into(),
        }],
        tools: Vec::new(),
        messages,
        breakpoints: Vec::new(),
        cache: CacheMode::Off,
        effort: None,
        thinking: None,
        max_tokens: 1024,
        format: None,
        purpose: Purpose::Turn,
    }
}

fn user(text: &str) -> Message {
    Message::User {
        blocks: vec![Block::Text { text: text.into() }],
    }
}

async fn generate(
    mock: &MockProvider,
    req: GenerateRequest,
) -> (Vec<GenItem>, Result<(), ProviderError>) {
    let (tx, rx) = mpsc::unbounded();
    let result = mock.generate(req, GenSink::new(tx)).await;
    (rx.collect().await, result)
}

fn transient() -> ProviderError {
    ProviderError {
        kind: ProviderErrorKind::Transient,
        http_status: Some(529),
        raw: "overloaded".into(),
        retry_after: None,
    }
}

#[tokio::test]
async fn scripts_play_in_order_and_requests_are_recorded() {
    let mock = MockProvider::strict();
    mock.push(Script::reply("one"))
        .push(Script::new().text("par").fail(transient()))
        .push(vec![GenItem::TextDelta { text: "raw".into() }])
        .push(transient());
    let mut seen = mock.seen();

    let (items, result) = generate(&mock, request(vec![user("a")])).await;
    result.unwrap();
    assert_eq!(
        items,
        [
            GenItem::TextDelta { text: "one".into() },
            GenItem::Stop(Stop {
                reason: StopReason::EndTurn,
                details: None,
            }),
        ]
    );
    assert!(seen.has_changed().unwrap());
    assert_eq!(*seen.borrow_and_update(), 1);

    let (items, result) = generate(&mock, request(vec![user("b")])).await;
    assert_eq!(items, [GenItem::TextDelta { text: "par".into() }]);
    assert_eq!(result.unwrap_err().kind, ProviderErrorKind::Transient);

    let (items, result) = generate(&mock, request(vec![user("c")])).await;
    result.unwrap();
    assert_eq!(items, [GenItem::TextDelta { text: "raw".into() }]);

    let (items, result) = generate(&mock, request(vec![user("d")])).await;
    assert!(items.is_empty());
    assert_eq!(result.unwrap_err().kind, ProviderErrorKind::Transient);

    let (_, result) = generate(&mock, request(vec![user("e")])).await;
    assert_eq!(
        result.unwrap_err().kind,
        ProviderErrorKind::Invalid,
        "strict mock ran out"
    );

    let texts: Vec<_> = mock
        .requests()
        .iter()
        .map(|r| match &r.messages[0] {
            Message::User { blocks } => match &blocks[0] {
                Block::Text { text } => text.clone(),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(texts, ["a", "b", "c", "d", "e"]);
    assert_eq!(mock.scripts_left(), 0);
}

#[tokio::test]
async fn gate_holds_the_stream() {
    let mock = MockProvider::strict();
    let (script, gate) = Script::new().text("before").gate();
    mock.push(script.text("after").stop(StopReason::EndTurn));

    let (tx, mut rx) = mpsc::unbounded();
    let task = {
        let mock = mock.clone();
        tokio::spawn(async move {
            mock.generate(request(vec![user("x")]), GenSink::new(tx))
                .await
        })
    };
    assert_eq!(
        rx.next().await,
        Some(GenItem::TextDelta {
            text: "before".into()
        })
    );
    tokio::task::yield_now().await;
    assert!(
        futures::poll!(rx.next()).is_pending(),
        "gated item must wait"
    );
    gate.open();
    assert_eq!(
        rx.next().await,
        Some(GenItem::TextDelta {
            text: "after".into()
        })
    );
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn echo_when_scripts_run_out() {
    let mock = MockProvider::default();
    let (items, result) = generate(&mock, request(vec![user("hello there")])).await;
    result.unwrap();
    let text: String = items
        .iter()
        .filter_map(|i| match i {
            GenItem::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "[mock:mock] echo: hello there");
    assert!(
        matches!(items.last(), Some(GenItem::Stop(stop)) if stop.reason == StopReason::EndTurn)
    );
    assert!(
        items
            .iter()
            .any(|i| matches!(i, GenItem::Usage(Usage { output, .. }) if *output > 0))
    );

    let mut req = request(vec![user("plan")]);
    req.format = Some(eva_protocol::types::generate::OutputFormat {
        schema: json!({"type": "object", "properties": {"ops": {"type": "array"}}, "required": ["ops"]}),
        strict: true,
    });
    let (items, _) = generate(&mock, req).await;
    assert_eq!(
        items[0],
        GenItem::TextDelta {
            text: "{\"ops\":[]}".into()
        }
    );
}

async fn refusal(mock: &MockProvider, req: GenerateRequest) -> String {
    let (items, result) = generate(mock, req).await;
    assert!(items.is_empty());
    let error = result.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Invalid, "{}", error.raw);
    error.raw
}

fn tool_use(id: &str) -> Block {
    Block::ToolUse {
        id: id.into(),
        name: "fs_read".into(),
        input: json!({}),
    }
}

fn tool_result(id: &str) -> Block {
    Block::ToolResult {
        tool_use_id: id.into(),
        content: vec![ResultBlock::Text { text: "ok".into() }],
        is_error: false,
    }
}

fn system(text: &str) -> Message {
    Message::System {
        kind: SystemKind::NoteUpdated,
        blocks: vec![Block::Text { text: text.into() }],
        clear_at: None,
        output_config: None,
        tool_changes: None,
    }
}

#[tokio::test]
async fn invalid_requests_fail_loudly() {
    let mock = MockProvider::strict();
    let mut native = Config {
        on_exhausted: Exhausted::Fail,
        ..Config::default()
    };
    native.models[0].caps.system_in_messages = true;
    native.models[0].caps.cache_breakpoints = 4;
    let native = MockProvider::new(native);

    let mut empty_system = request(vec![user("hi")]);
    empty_system.system[0].text = "  ".into();
    assert!(
        refusal(&mock, empty_system)
            .await
            .contains("empty system prompt")
    );

    let unpaired = request(vec![
        user("hi"),
        Message::Assistant {
            blocks: vec![tool_use("t1")],
        },
        user("no result"),
    ]);
    assert!(refusal(&mock, unpaired).await.contains("no tool_result"));

    let orphan = request(vec![
        user("hi"),
        Message::User {
            blocks: vec![tool_result("t9")],
        },
    ]);
    assert!(refusal(&mock, orphan).await.contains("answers no tool_use"));

    let ends_with_assistant = request(vec![
        user("hi"),
        Message::Assistant {
            blocks: vec![Block::Text { text: "yes".into() }],
        },
    ]);
    assert!(
        refusal(&mock, ends_with_assistant)
            .await
            .contains("ends with an assistant")
    );

    let unsigned = request(vec![
        user("hi"),
        Message::Assistant {
            blocks: vec![
                Block::Thinking {
                    signature: Bytes::new(),
                    model: ModelId::from(CHAT_MODEL),
                    text: Some("hm".into()),
                },
                tool_use("t1"),
            ],
        },
        Message::User {
            blocks: vec![tool_result("t1")],
        },
    ]);
    assert!(
        refusal(&mock, unsigned)
            .await
            .contains("without a signature")
    );

    let without_capability = request(vec![user("hi"), system("[notes] saved")]);
    assert!(
        refusal(&mock, without_capability.clone())
            .await
            .contains("system_in_messages")
    );
    let (_, result) = generate(&native, without_capability).await;
    assert_eq!(
        result.unwrap_err().raw,
        "mock ran out of scripts for this request",
        "valid for a native model"
    );

    assert!(
        refusal(&native, request(vec![system("[x]"), user("hi")]))
            .await
            .contains("first message")
    );
    let after_assistant = request(vec![
        user("hi"),
        Message::Assistant {
            blocks: vec![Block::Text { text: "yes".into() }],
        },
        system("[x]"),
    ]);
    assert!(
        refusal(&native, after_assistant)
            .await
            .contains("right after an assistant")
    );
    assert!(
        refusal(&native, request(vec![user("a"), system("[x]"), user("b")]))
            .await
            .contains("right before a user")
    );

    let mut deferred = request(vec![user("hi")]);
    deferred.tools.push(ToolSpec {
        name: "cron_create".into(),
        description: String::new(),
        input_schema: json!({}),
        deferred: true,
    });
    assert!(refusal(&mock, deferred).await.contains("tool_changes"));

    let mut breakpoints = request(vec![user("hi")]);
    breakpoints.breakpoints = vec![Position::Message { index: 3 }];
    assert!(refusal(&native, breakpoints).await.contains("out of range"));

    let mut keep_alive = request(vec![user("hi")]);
    keep_alive.purpose = Purpose::KeepAlive;
    assert!(
        refusal(&mock, keep_alive)
            .await
            .contains("cache_keep_alive")
    );

    let mut unknown = request(vec![user("hi")]);
    unknown.model = ModelId::from("nope");
    let (_, result) = generate(&mock, unknown).await;
    assert_eq!(result.unwrap_err().kind, ProviderErrorKind::NotFound);

    assert_eq!(
        mock.requests().len(),
        9,
        "rejected requests are recorded too"
    );
}

#[tokio::test]
async fn embed_and_count_tokens() {
    let mock = MockProvider::default();
    let embed = |inputs: &[&str]| EmbedRequest {
        model: ModelId::from(EMBED_MODEL),
        inputs: inputs.iter().map(|s| (*s).to_owned()).collect(),
    };
    let first = mock.embed(embed(&["alpha", "beta"])).await.unwrap();
    let again = mock.embed(embed(&["alpha"])).await.unwrap();
    assert_eq!(first.vectors.len(), 2);
    assert_eq!(first.vectors[0].len(), 64);
    assert_eq!(first.vectors[0], again.vectors[0], "deterministic");
    assert_ne!(first.vectors[0], first.vectors[1]);
    let norm: f32 = first.vectors[0].iter().map(|x| x * x).sum();
    assert!((norm - 1.0).abs() < 1e-4);
    assert_eq!(first.usage.input, 3);

    let chat = EmbedRequest {
        model: ModelId::from(CHAT_MODEL),
        inputs: vec!["x".into()],
    };
    assert_eq!(
        mock.embed(chat).await.unwrap_err().kind,
        ProviderErrorKind::Invalid
    );

    // «You are Eva.» (12) + «0123456789abcdef» (16) = 28 байт → 7 токенов.
    let tokens = mock
        .count_tokens(request(vec![user("0123456789abcdef")]))
        .await
        .unwrap();
    assert_eq!(tokens, 7);
    assert!(mock.decl().count_tokens && mock.decl().local);
}

#[test]
fn config_defaults_and_explicit_empty_models() {
    let config: Config = serde_json::from_value(json!({})).unwrap();
    assert_eq!(config.models.len(), 2);
    assert_eq!(config.on_exhausted, Exhausted::Echo);

    let config: Config =
        serde_json::from_value(json!({"models": [], "on_exhausted": "fail"})).unwrap();
    assert!(config.models.is_empty(), "an empty list is empty (I8)");
    assert_eq!(config.on_exhausted, Exhausted::Fail);
}

/// Без отложенного каталога дочитывать нечего; с ним `settle` ждёт `list`,
/// и модели каталога встают рядом с моделями конфига.
#[tokio::test]
async fn settle_installs_a_late_listing() {
    assert!(!MockProvider::default().settle().await, "nothing to settle");

    let mock = MockProvider::default().awaiting_listing();
    let settling = tokio::spawn({
        let mock = mock.clone();
        async move { mock.settle().await }
    });
    tokio::task::yield_now().await;
    assert!(!settling.is_finished(), "settle waits for the listing");
    assert_eq!(mock.models().len(), 2);

    let late = eva_protocol::types::model::ModelDecl {
        name: ModelId::from("late"),
        ..mock.models()[0].clone()
    };
    mock.list(vec![late]);
    assert!(settling.await.expect("settle task"), "the registry changed");
    let names: Vec<String> = mock
        .models()
        .into_iter()
        .map(|m| m.name.as_str().to_owned())
        .collect();
    assert_eq!(names, [CHAT_MODEL, EMBED_MODEL, "late"]);
    assert!(!mock.settle().await, "settled once");
}
