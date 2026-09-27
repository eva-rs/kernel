//! Билдер → capability в `hello`.

use eva::data;
use serde_json::json;

use eva_protocol::{
    types::tool::Need,
    wire::{capabilities::FailMode, events::Topic},
};
use eva_sdk::{
    BeforeSend, Decision, HookPoint, Module, ToolContent, ToolCtx, ToolError, ToolResult,
    tool::input_schema,
};

/// Send a message to the chat.
#[data]
pub struct SendInput {
    /// External chat id.
    pub chat: String,
    pub text: String,
    pub silent: Option<bool>,
    pub target: Target,
}

#[data]
pub struct Target {
    /// Message to reply to.
    pub reply_to: i64,
}

async fn send(_: ToolCtx, input: SendInput) -> Result<ToolResult, ToolError> {
    Ok(ToolResult::ok(vec![ToolContent::text(input.text)]))
}

#[test]
fn empty_module_declares_nothing() {
    let caps = Module::new().capabilities();
    assert!(caps.provider.is_none());
    assert!(caps.tools.is_none());
    assert!(caps.events.is_none());
    assert!(caps.hooks.is_none());
    assert!(!caps.ask);
    assert!(caps.names().is_empty());
}

#[test]
fn ask_is_a_capability() {
    let caps = Module::new().ask(true).capabilities();
    assert!(caps.ask);
    assert_eq!(caps.names(), ["ask"]);
}

#[test]
fn schema_is_inline_without_title() {
    let schema = input_schema::<SendInput>();
    let text = schema.to_string();
    assert!(!text.contains("\"title\""), "{text}");
    assert!(!text.contains("$ref"), "{text}");
    assert!(!text.contains("$defs"), "{text}");
    assert!(schema.get("$schema").is_none());
    assert_eq!(schema["type"], json!("object"));
    let required = schema["required"].as_array().unwrap();
    for field in ["chat", "text", "target"] {
        assert!(
            required.contains(&json!(field)),
            "{field} is required: {text}"
        );
    }
    assert!(!required.contains(&json!("silent")));
    assert_eq!(
        schema["properties"]["target"]["properties"]["reply_to"]["type"],
        json!("integer")
    );
}

#[test]
fn builder_fills_every_section() {
    let caps = Module::new()
        .group("x", "Messenger X actions")
        .tool("x.send", send)
        .ends_turn(true)
        .master_only(true)
        .trusted_only(true)
        .needs(&[Need::Hands])
        .idempotent(true)
        .tool("x.other", send)
        .description("Another tool.")
        .on_event(Topic::ChatDeleted, |_, _| async {})
        .on_event(Topic::ChatCreated, |_, _| async {})
        .on_event(Topic::ChatDeleted, |_, _| async { Ok::<(), String>(()) })
        .hook(HookPoint::BeforeSend, |_, _: BeforeSend| async {
            Decision::Continue
        })
        .hooks_fail_mode(FailMode::Closed)
        .capabilities();

    let tools = caps.tools.expect("tools section");
    assert_eq!(tools.groups.len(), 1);
    assert_eq!(tools.tools.len(), 2);
    let first = &tools.tools[0];
    assert_eq!(first.path, "x.send");
    assert_eq!(first.description, "Send a message to the chat.");
    assert!(first.ends_turn && first.master_only && first.trusted_only && first.idempotent);
    assert_eq!(first.needs, vec![Need::Hands]);
    assert!(first.input.get("description").is_none());
    let second = &tools.tools[1];
    assert_eq!(second.description, "Another tool.");
    assert!(!second.ends_turn && !second.master_only);

    let events = caps.events.expect("events section");
    assert_eq!(
        events.subscriptions,
        vec![Topic::ChatDeleted, Topic::ChatCreated]
    );

    let hooks = caps.hooks.expect("hooks section");
    assert_eq!(hooks.points, vec![HookPoint::BeforeSend]);
    assert_eq!(hooks.fail_mode, FailMode::Closed);
    assert!(caps.provider.is_none());
}

#[test]
#[should_panic(expected = "does not match")]
fn hook_point_must_match_input() {
    _ = Module::new().hook(HookPoint::BeforeTurn, |_, _: BeforeSend| async {
        Decision::Continue
    });
}

#[test]
#[should_panic(expected = "declared twice")]
fn tool_path_is_unique() {
    _ = Module::new().tool("x.send", send).tool("x.send", send);
}
