//! I1 на адаптере (`01 §12`): рендер префикса нормализованных сообщений —
//! префикс vendor-`messages` рендера длинного запроса; разница допустима только
//! в части, несущей хвостовой breakpoint короткого запроса.

use proptest::prelude::*;
use serde_json::{Value, json};

use eva_protocol::types::{
    Bytes, ModelId,
    generate::{
        Block, CacheMode, GenerateRequest, Message, Position, Purpose, ResultBlock, SystemKind,
        SystemSection,
    },
    model::ModelDecl,
};
use eva_provider_openai::{Config, Dialect, declare, render};

#[derive(Debug, Clone)]
enum Turn {
    User {
        texts: Vec<String>,
        image: bool,
    },
    Assistant {
        text: String,
    },
    Tools {
        /// `Some(true)` — thinking этой модели, `Some(false)` — чужой.
        thinking: Option<bool>,
        text: Option<String>,
        /// `(is_error, image)` на вызов.
        calls: Vec<(bool, bool)>,
        follow: Option<String>,
    },
    System {
        text: String,
    },
}

fn word() -> impl Strategy<Value = String> {
    "\\PC{1,12}"
}

fn turn() -> impl Strategy<Value = Turn> {
    prop_oneof![
        (prop::collection::vec(word(), 1..3), any::<bool>())
            .prop_map(|(texts, image)| Turn::User { texts, image }),
        word().prop_map(|text| Turn::Assistant { text }),
        (
            prop::option::of(any::<bool>()),
            prop::option::of(word()),
            prop::collection::vec((any::<bool>(), any::<bool>()), 1..3),
            prop::option::of(word()),
        )
            .prop_map(|(thinking, text, calls, follow)| Turn::Tools {
                thinking,
                text,
                calls,
                follow,
            }),
        word().prop_map(|text| Turn::System { text }),
    ]
}

struct Variant {
    model: ModelDecl,
    dialect: Dialect,
}

fn variants() -> Vec<Variant> {
    let listing: Value =
        serde_json::from_str(include_str!("fixtures/openrouter_models.json")).unwrap();
    let decls = declare(Some(&listing), &Config::default());
    let find = |name: &str| {
        decls
            .iter()
            .find(|m| m.name.as_str() == name)
            .unwrap()
            .clone()
    };

    let mut sonnet = find("anthropic/claude-sonnet-4.5");
    sonnet.caps.system_in_messages = true;
    vec![
        Variant {
            model: sonnet,
            dialect: Dialect::Openrouter,
        },
        Variant {
            model: find("anthropic/claude-sonnet-4.5"),
            dialect: Dialect::Openrouter,
        },
        Variant {
            model: find("openai/gpt-5-mini"),
            dialect: Dialect::Openrouter,
        },
        Variant {
            model: find("google/gemini-2.5-flash-image"),
            dialect: Dialect::Openrouter,
        },
        Variant {
            model: find("deepseek/deepseek-r1"),
            dialect: Dialect::Plain,
        },
    ]
}

fn build(turns: &[Turn], variant: &Variant) -> Vec<Message> {
    let model = &variant.model.name;
    let png = || Bytes(vec![0x89, b'P', b'N', b'G']);
    let mut out = Vec::new();
    let mut call = 0;
    for turn in turns {
        match turn {
            Turn::User { texts, image } => {
                let mut blocks: Vec<Block> = texts
                    .iter()
                    .map(|t| Block::Text { text: t.clone() })
                    .collect();
                if *image {
                    blocks.push(Block::Image {
                        media_type: "image/png".into(),
                        data: png(),
                    });
                }
                out.push(Message::User { blocks });
            }
            Turn::Assistant { text } => out.push(Message::Assistant {
                blocks: vec![Block::Text { text: text.clone() }],
            }),
            Turn::Tools {
                thinking,
                text,
                calls,
                follow,
            } => {
                let mut said = Vec::new();
                if let Some(own) = thinking {
                    let from = if *own {
                        model.clone()
                    } else {
                        ModelId::from("other/model")
                    };
                    let detail = json!({"type": "reasoning.text", "text": "plan", "signature": format!("sig{call}")});
                    said.push(Block::Thinking {
                        signature: Bytes(serde_json::to_vec(&detail).unwrap()),
                        model: from,
                        text: Some("plan".into()),
                    });
                }
                if let Some(text) = text {
                    said.push(Block::Text { text: text.clone() });
                }
                let mut results = Vec::new();
                for &(is_error, image) in calls {
                    let id = format!("call_{call}");
                    call += 1;
                    said.push(Block::ToolUse {
                        id: id.clone(),
                        name: "fs_read".into(),
                        input: json!({"n": call}),
                    });
                    let mut content = vec![ResultBlock::Text {
                        text: format!("[t{call}]\nresult"),
                    }];
                    if image {
                        content.push(ResultBlock::Image {
                            media_type: "image/png".into(),
                            data: png(),
                        });
                    }
                    results.push(Block::ToolResult {
                        tool_use_id: id,
                        content,
                        is_error,
                    });
                }
                if let Some(follow) = follow {
                    results.push(Block::Text {
                        text: follow.clone(),
                    });
                }
                out.push(Message::Assistant { blocks: said });
                out.push(Message::User { blocks: results });
            }
            Turn::System { text } if variant.model.caps.system_in_messages => {
                out.push(Message::System {
                    kind: SystemKind::Reminder,
                    blocks: vec![Block::Text { text: text.clone() }],
                    clear_at: None,
                    output_config: None,
                    tool_changes: None,
                })
            }
            Turn::System { .. } => {}
        }
    }
    out
}

fn request(
    model: &ModelId,
    messages: Vec<Message>,
    middle: Option<usize>,
    cache: CacheMode,
) -> GenerateRequest {
    let last = messages.len() - 1;
    let mut breakpoints = vec![Position::System];
    breakpoints.extend(
        middle
            .filter(|&m| m < last)
            .map(|m| Position::Message { index: m as u32 }),
    );
    breakpoints.push(Position::Message { index: last as u32 });
    GenerateRequest {
        model: model.clone(),
        system: vec![SystemSection {
            name: "persona".into(),
            text: "[persona]\nYou are Eva.".into(),
        }],
        tools: Vec::new(),
        messages,
        breakpoints,
        cache,
        effort: None,
        thinking: None,
        max_tokens: 1024,
        format: None,
        purpose: Purpose::Turn,
    }
}

fn messages(req: &GenerateRequest, variant: &Variant) -> Vec<Value> {
    let body = render(req, &variant.model, variant.dialect).unwrap();
    body["messages"].as_array().unwrap().clone()
}

/// Снимает `cache_control` и возвращает голую текстовую часть в строку — ровно то, что добавил хвостовой breakpoint.
fn unmark(message: &Value) -> Value {
    let mut message = message.clone();
    if let Some(parts) = message["content"].as_array_mut() {
        for part in parts.iter_mut() {
            if let Some(part) = part.as_object_mut() {
                part.remove("cache_control");
            }
        }
        if let [Value::Object(part)] = parts.as_slice()
            && part.len() == 2
            && part["type"] == "text"
        {
            message["content"] = part["text"].clone();
        }
    }
    message
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn prefix_renders_to_prefix(
        turns in prop::collection::vec(turn(), 1..10),
        variant in 0..5_usize,
        cut in any::<prop::sample::Index>(),
        middle in any::<prop::sample::Index>(),
        long in any::<bool>(),
    ) {
        let variant = &variants()[variant];
        let full = build(&turns, variant);
        prop_assume!(!full.is_empty());
        let n = cut.index(full.len()) + 1;
        let middle = (n > 1).then(|| middle.index(n - 1));
        let cache = if long { CacheMode::Long } else { CacheMode::Short };

        let whole = n == full.len();
        let short = messages(&request(&variant.model.name, full[..n].to_vec(), middle, cache), variant);
        let long = messages(&request(&variant.model.name, full, middle, cache), variant);

        if whole {
            prop_assert_eq!(short, long);
            return Ok(());
        }
        prop_assert!(short.len() < long.len());
        let (tail, head) = short.split_last().unwrap();
        prop_assert_eq!(head, &long[..head.len()]);
        prop_assert_eq!(unmark(tail), long[head.len()].clone());
    }
}

#[test]
fn trailing_breakpoint_is_the_only_difference() {
    let variant = &variants()[1];
    let turns = [
        Turn::User {
            texts: vec!["hi".into()],
            image: false,
        },
        Turn::Assistant {
            text: "hello".into(),
        },
        Turn::User {
            texts: vec!["more".into()],
            image: false,
        },
    ];
    let full = build(&turns, variant);
    let short = messages(
        &request(
            &variant.model.name,
            full[..1].to_vec(),
            None,
            CacheMode::Short,
        ),
        variant,
    );
    let long = messages(
        &request(&variant.model.name, full, None, CacheMode::Short),
        variant,
    );
    assert_eq!(
        short[1]["content"],
        json!([{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}])
    );
    assert_eq!(long[1]["content"], json!("hi"));
    assert_eq!(short[0], long[0], "system breakpoint is stable");
}
