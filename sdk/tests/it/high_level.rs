use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use eva::{data, time::HumanDuration};
use eva_protocol::wire::tools::{ToolCall, ToolItem};
use eva_rpc::{Config, Conn};
use eva_sdk::{
    Description, Group, Module, Need, ResultForm, Tool, ToolCtx, ToolError, include_translations,
    module, msg,
};
use futures::StreamExt as _;

use crate::fake::{self, Surfaces};

/// Repeat the given text.
#[data]
struct Say {
    text: String,
}

async fn say(ctx: ToolCtx<AtomicUsize>, input: Say) -> Result<String, ToolError> {
    ctx.state().fetch_add(1, Ordering::SeqCst);
    ctx.progress(msg!("Echoing {text}", text = &input.text))
        .await;
    Ok(input.text)
}

#[test]
fn metadata_uses_callers_package_and_can_be_overridden() {
    let m = module!();
    assert_eq!(m.module_name(), env!("CARGO_PKG_NAME"));
    assert_eq!(m.module_version(), env!("CARGO_PKG_VERSION"));
    let m = m.version("1.4.3").name("owm");
    assert_eq!(m.module_name(), "owm");
    assert_eq!(m.module_version(), "1.4.3");
}

#[test]
fn nested_groups_keep_local_names_and_tool_options() {
    let module = Module::new()
        .state(AtomicUsize::new(0))
        .group(
            Group::new("echo")
                .about("Echo")
                .tool(Tool::new("say", say).idempotent(false))
                .group(Group::new("nested").tool(Tool::new("say", say).parallel_safe(false))),
        )
        .group(Group::new("other").tool(Tool::new("say", say)));
    let tools = module.capabilities().tools.unwrap();
    assert_eq!(
        tools
            .groups
            .iter()
            .map(|g| g.path.as_str())
            .collect::<Vec<_>>(),
        ["echo", "echo.nested", "other"]
    );
    assert_eq!(
        tools
            .tools
            .iter()
            .map(|t| t.path.as_str())
            .collect::<Vec<_>>(),
        ["echo.say", "echo.nested.say", "other.say"]
    );
    assert!(!tools.tools[0].idempotent && tools.tools[0].parallel_safe);
    assert!(tools.tools[1].idempotent && !tools.tools[1].parallel_safe);
    assert!(tools.tools[2].idempotent && tools.tools[2].parallel_safe);
}

#[test]
fn description_and_individual_setters_produce_the_same_declaration() {
    let timeout = HumanDuration(Duration::from_secs(30));
    let description = Description {
        text: Some("Configured tool".into()),
        master_only: true,
        trusted_only: true,
        needs: vec![Need::Private],
        ends_turn: true,
        parallel_safe: false,
        waits_on_cancel: true,
        result: ResultForm::Uncapped,
        timeout: Some(timeout),
        idempotent: false,
    };
    let described = Module::new()
        .state(AtomicUsize::new(0))
        .group(Group::new("echo").tool(Tool::new("say", say).describe(description.clone())))
        .capabilities();
    let setters = Module::new()
        .state(AtomicUsize::new(0))
        .group(
            Group::new("echo").tool(
                Tool::new("say", say)
                    .description("Configured tool")
                    .master_only(true)
                    .trusted_only(true)
                    .needs(&[Need::Private])
                    .ends_turn(true)
                    .parallel_safe(false)
                    .waits_on_cancel(true)
                    .result(ResultForm::Uncapped)
                    .timeout(timeout)
                    .idempotent(false),
            ),
        )
        .capabilities();
    let module_setters = Module::new()
        .state(AtomicUsize::new(0))
        .group_path("echo", "")
        .tool_path("echo.say", say)
        .description("Configured tool")
        .master_only(true)
        .trusted_only(true)
        .needs(&[Need::Private])
        .ends_turn(true)
        .parallel_safe(false)
        .waits_on_cancel(true)
        .result(ResultForm::Uncapped)
        .timeout(timeout)
        .idempotent(false)
        .capabilities();
    let module_described = Module::new()
        .state(AtomicUsize::new(0))
        .group_path("echo", "")
        .tool_path("echo.say", say)
        .describe(description)
        .capabilities();
    assert_eq!(described, setters);
    assert_eq!(described, module_setters);
    assert_eq!(described, module_described);
}

#[test]
fn describe_replaces_options_and_setters_only_change_their_field() {
    let previous = Description {
        text: Some("Old description".into()),
        master_only: true,
        needs: vec![Need::Private],
        idempotent: false,
        timeout: Some(HumanDuration(Duration::from_secs(30))),
        ..Default::default()
    };
    let tools = Module::new()
        .state(AtomicUsize::new(0))
        .group(
            Group::new("echo")
                .tool(
                    Tool::new("replaced", say)
                        .describe(previous.clone())
                        .describe(Description {
                            parallel_safe: false,
                            ..Default::default()
                        }),
                )
                .tool(
                    Tool::new("updated", say)
                        .describe(previous)
                        .parallel_safe(false),
                )
                .tool(Tool::new("default", say)),
        )
        .capabilities()
        .tools
        .unwrap()
        .tools;
    let replaced = &tools[0];
    let updated = &tools[1];
    let mut expected = tools[2].clone();
    expected.path = replaced.path.clone();
    expected.parallel_safe = false;
    assert_eq!(replaced, &expected);
    assert_eq!(replaced.description, "Repeat the given text.");
    assert_eq!(updated.description, "Old description");
    assert!(updated.master_only);
    assert_eq!(updated.needs, [Need::Private]);
    assert!(updated.timeout.is_some());
    assert!(!updated.idempotent && !updated.parallel_safe);
    assert_eq!(updated.input, replaced.input);
}

#[test]
#[should_panic(expected = "canonical tool name collision")]
fn canonical_collisions_are_rejected_before_connecting() {
    let _ = Module::new()
        .state(AtomicUsize::new(0))
        .group(Group::new("a_b").tool(Tool::new("c", say)))
        .group(Group::new("a").tool(Tool::new("b_c", say)));
}

#[tokio::test]
async fn state_and_localized_progress_survive_rpc() {
    let module = module!()
        .name("echo")
        .state(AtomicUsize::new(0))
        .translations(include_translations!("examples/locales"))
        .group(Group::new("echo").tool(Tool::new("say", say)));
    let (module_end, soul_end) = Conn::pair(&Config::default());
    let soul = tokio::spawn(fake::accept(
        soul_end,
        fake::welcome("s1", &[("echo.say", 7)]),
        Surfaces::default(),
    ));
    let _connected = module
        .handshake(module_end, &fake::token("token"))
        .await
        .unwrap();
    let soul = soul.await.unwrap();
    let call = ToolCall {
        call_id: "localized".into(),
        tool_id: None,
        canonical: "echo_say".into(),
        input: serde_json::json!({"text":"hello"}),
        ctx: fake::turn(),
    };
    let items: Vec<_> = soul
        .module()
        .call_tool(&call)
        .await
        .unwrap()
        .collect()
        .await;
    let ToolItem::Progress {
        text,
        message: Some(message),
    } = items[0].as_ref().unwrap()
    else {
        panic!("localized progress expected");
    };
    assert_eq!(text, "Echoing hello");
    assert_eq!(message.render("ru-RU"), "Повторяю hello");
    assert_eq!(message.render("de"), "Echoing hello");
    assert_eq!(message.domain, "echo");
    let ToolItem::Result(result) = items[1].as_ref().unwrap() else {
        panic!("result expected");
    };
    assert!(!result.is_error);
}

#[test]
fn plurals_context_and_english_fallback() {
    let catalogs = include_translations!("examples/locales");
    for (n, suffix) in [
        (1, "файл"),
        (2, "файла"),
        (5, "файлов"),
        (11, "файлов"),
        (21, "файл"),
    ] {
        let message = msg!(
            "Found {count} file",
            plural = "Found {count} files",
            count = n
        );
        assert!(
            catalogs
                .render(message.clone(), "ru_RU.UTF-8")
                .ends_with(suffix)
        );
        assert_eq!(
            catalogs.render(message, "zz"),
            if n == 1 {
                "Found 1 file".into()
            } else {
                format!("Found {n} files")
            }
        );
    }
    assert_eq!(
        catalogs.render(msg!("Open", context = "button"), "ru"),
        "Открыть"
    );
    assert_eq!(catalogs.render(msg!("Open"), "ru"), "Open");
}

#[tokio::test]
async fn uninitialized_state_is_rejected_before_binding() {
    #[data]
    struct Config {
        prefix: String,
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("module.sock");
    let config: eva_sdk::ServiceConfig = serde_json::from_value(serde_json::json!({
        "listen": {"uds":path}, "key":{"plaintext":"test-key"}
    }))
    .unwrap();
    let result = module!()
        .init("echo", async |cfg: Config| Ok(cfg.prefix))
        .listen(&config)
        .await;
    assert!(
        matches!(result, Err(eva_sdk::ConnectError::Config(error)) if error.contains("not initialized"))
    );
    assert!(!path.exists());
}
