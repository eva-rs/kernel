//! Реестр: каталог OpenRouter + поправки конфига → `ModelDecl`.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use eva_core::Provider;
use eva_protocol::types::{
    Effort, ModelId,
    model::{EmbedDecl, ModelDecl, Price},
};
use eva_provider_openai::{CapsOverride, Config, Dialect, ModelOverride, OpenAiProvider, declare};

fn listing() -> Value {
    serde_json::from_str(include_str!("fixtures/openrouter_models.json")).unwrap()
}

fn by_name(decls: Vec<ModelDecl>) -> BTreeMap<String, ModelDecl> {
    decls.into_iter().map(|d| (d.name.0.clone(), d)).collect()
}

#[test]
fn openrouter_catalog_maps_to_decls() {
    let decls = declare(Some(&listing()), &Config::default());
    insta::assert_json_snapshot!("openrouter_catalog", decls);

    let models = by_name(decls);
    assert_eq!(models.len(), 7);

    let sonnet = &models["anthropic/claude-sonnet-4.5"];
    assert_eq!(
        (sonnet.context_window, sonnet.max_output),
        (1_000_000, 64_000)
    );
    assert_eq!(
        sonnet.price,
        Some(Price {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write_short: 3.75,
            cache_write_long: 6.0,
        })
    );
    let caps = &sonnet.caps;
    assert!(caps.vision && caps.thinking && caps.structured_output && !caps.image_out);
    assert_eq!(
        caps.effort_levels,
        [Effort::Low, Effort::Medium, Effort::High]
    );
    assert_eq!((caps.cache_breakpoints, caps.cache_min_prefix), (4, 1024));
    assert!(caps.thinking_binding);
    assert!(!caps.system_in_messages && !caps.tool_changes && !caps.clear_at);
    assert!(!caps.effort_in_messages && !caps.context_editing);
    assert!(!caps.server_compaction && !caps.cache_keep_alive);

    let mini = &models["openai/gpt-5-mini"];
    assert_eq!(
        mini.caps.cache_breakpoints, 0,
        "openai caches automatically"
    );
    assert!(!mini.caps.thinking_binding);
    let price = mini.price.unwrap();
    assert_eq!(
        (price.input, price.output, price.cache_read),
        (0.25, 2.0, 0.025)
    );
    assert_eq!(
        price.cache_write_short, 0.25,
        "no write surcharge without a listed price"
    );

    let pro = &models["google/gemini-2.5-pro"];
    assert_eq!(pro.caps.cache_breakpoints, 4);
    assert_eq!(pro.price.unwrap().cache_write_long, 1.625);

    let image = &models["google/gemini-2.5-flash-image"];
    assert!(image.caps.image_out && image.caps.vision && !image.caps.thinking);
    assert!(image.caps.effort_levels.is_empty());

    let r1 = &models["deepseek/deepseek-r1"];
    assert!(r1.caps.thinking && !r1.caps.vision && !r1.caps.structured_output);
    assert_eq!(
        r1.max_output, 163_840,
        "no max_completion_tokens: the whole window"
    );
    assert_eq!(r1.caps.cache_min_prefix, 0);

    assert_eq!(
        models["openrouter/auto"].price, None,
        "negative price means variable"
    );
    assert!(!models["meta-llama/llama-3.3-70b-instruct"].caps.thinking);
}

#[test]
fn allow_list_and_overrides() {
    let config = Config {
        allow: Some(vec![
            "anthropic/claude-sonnet-4.5".into(),
            "openai/gpt-5-mini".into(),
            "local/qwen3".into(),
            "missing/model".into(),
        ]),
        models: BTreeMap::from([
            (
                ModelId::from("openai/gpt-5-mini"),
                ModelOverride {
                    max_output: Some(32_000),
                    caps: CapsOverride {
                        system_in_messages: Some(true),
                        effort_levels: Some(vec![Effort::Low, Effort::High, Effort::Max]),
                        ..CapsOverride::default()
                    },
                    ..ModelOverride::default()
                },
            ),
            (
                ModelId::from("anthropic/claude-sonnet-4.5"),
                ModelOverride {
                    context_window: Some(200_000),
                    price: Some(Price {
                        input: 1.0,
                        output: 2.0,
                        cache_read: 0.1,
                        cache_write_short: 1.25,
                        cache_write_long: 2.0,
                    }),
                    caps: CapsOverride {
                        cache_breakpoints: Some(2),
                        thinking_binding: Some(false),
                        ..CapsOverride::default()
                    },
                    ..ModelOverride::default()
                },
            ),
            (
                ModelId::from("local/qwen3"),
                ModelOverride {
                    context_window: Some(32_768),
                    caps: CapsOverride {
                        thinking: Some(true),
                        ..CapsOverride::default()
                    },
                    ..ModelOverride::default()
                },
            ),
        ]),
        ..Config::default()
    };
    let models = by_name(declare(Some(&listing()), &config));
    assert_eq!(
        models.keys().collect::<Vec<_>>(),
        [
            "anthropic/claude-sonnet-4.5",
            "local/qwen3",
            "openai/gpt-5-mini"
        ]
    );

    let mini = &models["openai/gpt-5-mini"];
    assert_eq!((mini.context_window, mini.max_output), (400_000, 32_000));
    assert!(mini.caps.system_in_messages);
    assert_eq!(
        mini.caps.effort_levels,
        [Effort::Low, Effort::High, Effort::Max]
    );

    let sonnet = &models["anthropic/claude-sonnet-4.5"];
    assert_eq!(sonnet.context_window, 200_000);
    assert_eq!(sonnet.price.unwrap().input, 1.0);
    assert_eq!(sonnet.caps.cache_breakpoints, 2);
    assert!(!sonnet.caps.thinking_binding);

    let qwen = &models["local/qwen3"];
    assert_eq!((qwen.context_window, qwen.max_output), (32_768, 32_768));
    assert!(qwen.caps.thinking);
    assert_eq!(
        qwen.caps.effort_levels,
        [Effort::Low, Effort::Medium, Effort::High]
    );
    assert_eq!(qwen.price, None);
}

#[test]
fn plain_server_lives_on_config() {
    let config: Config = serde_json::from_value(json!({
        "base_url": "http://127.0.0.1:8080/v1",
        "dialect": "plain",
        "models": {
            "qwen3-32b": {
                "context_window": 40960,
                "max_output": 8192,
                "caps": {"thinking": true, "cache_breakpoints": 4, "cache_min_prefix": 512}
            },
            "no-window": {"max_output": 100},
            "bge-m3": {"embed": {"dims": 1024, "max_input_tokens": 8192}}
        }
    }))
    .unwrap();
    assert_eq!(config.dialect, Dialect::Plain);

    let vllm = json!({"object": "list", "data": [{"id": "qwen3-32b", "max_model_len": 32768}]});
    for listing in [None, Some(&vllm)] {
        let models = by_name(declare(listing, &config));
        assert_eq!(models.keys().collect::<Vec<_>>(), ["bge-m3", "qwen3-32b"]);

        let qwen = &models["qwen3-32b"];
        assert_eq!(qwen.context_window, 40_960, "config wins over the server");
        assert_eq!(qwen.caps.cache_breakpoints, 0, "plain has no cache_control");
        assert_eq!(qwen.caps.cache_min_prefix, 512);
        assert!(qwen.caps.thinking && !qwen.caps.thinking_binding);

        let bge = &models["bge-m3"];
        assert_eq!(
            bge.embed,
            Some(EmbedDecl {
                dims: 1024,
                max_input_tokens: 8192
            })
        );
        assert_eq!((bge.context_window, bge.max_output), (8192, 0));
    }

    let listed_only = Config {
        dialect: Dialect::Plain,
        ..Config::default()
    };
    let models = by_name(declare(Some(&vllm), &listed_only));
    assert_eq!(
        models["qwen3-32b"].context_window, 32_768,
        "vLLM max_model_len"
    );
}

#[test]
fn caps_override_rejects_forms_chat_completions_cannot_express() {
    let config = serde_json::from_value::<Config>(json!({
        "models": {"x/y": {"context_window": 1000, "caps": {"clear_at": true}}}
    }));
    let error = config.unwrap_err().to_string();
    assert!(error.contains("clear_at"), "{error}");
}

#[test]
fn config_defaults_and_hidden_key() {
    let config: Config =
        serde_json::from_value(json!({"api_key": {"plaintext": "sk-or-secret"}})).unwrap();
    assert_eq!(config.base_url, "https://openrouter.ai/api/v1");
    assert_eq!(config.dialect, Dialect::Openrouter);
    assert_eq!(config.timeout.as_secs(), 300);
    assert_eq!(
        config.api_key.as_ref().unwrap().read().unwrap(),
        "sk-or-secret"
    );
    assert!(!format!("{config:?}").contains("sk-or-secret"));

    let file: Config =
        serde_json::from_value(json!({"api_key": {"file": "/run/credentials/eva/openrouter"}}))
            .unwrap();
    assert!(matches!(
        file.api_key,
        Some(eva::utils::SecretString::File(_))
    ));
}

#[test]
fn decl_is_local_only_on_loopback() {
    _ = rustls::crypto::ring::default_provider().install_default();
    let local = |base_url: &str| {
        let config = Config {
            base_url: base_url.into(),
            ..Config::default()
        };
        let provider = OpenAiProvider::from_listing(config, Some(&listing())).unwrap();
        let decl = provider.decl();
        assert!(!decl.count_tokens);
        decl.local
    };
    assert!(!local("https://openrouter.ai/api/v1"));
    assert!(local("http://127.0.0.1:8080/v1"));
    assert!(local("http://localhost:11434/v1"));
    assert!(local("http://[::1]:8000/v1"));
    assert!(!local("http://192.168.1.10:8000/v1"));

    let provider = OpenAiProvider::from_listing(Config::default(), Some(&listing())).unwrap();
    assert_eq!(provider.models().len(), 7);
    assert!(
        OpenAiProvider::from_listing(
            Config {
                base_url: "not a url".into(),
                ..Config::default()
            },
            None
        )
        .is_err()
    );
}
