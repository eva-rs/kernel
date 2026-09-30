//! Минимальный сервис: своя surface, счётчик запусков в KV, подписка на
//! события своих чатов и один тул. Модуль слушает, ядро набирает его само —
//! адреса ядра модуль не знает. Конфиг — секция `module`, например:
//!
//! ```yaml
//! module:
//!   listen: { uds: /run/eva-echo/module.sock }
//!   key: { file: /run/credentials/eva-echo.service/key }
//! ```
//!
//! У ядра — запись с тем же секретом:
//!
//! ```yaml
//! modules:
//!   services:
//!     echo:
//!       address: uds:///run/eva-echo/module.sock
//!       key: { file: /run/credentials/eva-kernel.service/echo-key }
//! ```
//!
//! Запуск: `cargo run -p eva-sdk --example echo -- --config echo.yaml`.

use eva::{
    cli::{App, NoArgs},
    component_configs::ComponentConfigs,
    data,
};
use eva_sdk::{
    Event, Kernel, Module, ServiceConfig, ToolContent, ToolCtx, ToolError, ToolResult, Topic,
    prelude::*,
    protocol::{
        requests::{kv, surfaces},
        types::Value,
    },
};

/// Surface — slug `[a-z][a-z0-9_]{0,31}`, не id модуля.
const SURFACE: &str = "echo";

/// Repeat the given text back.
#[data]
pub struct SayInput {
    /// Text to repeat.
    pub text: String,
}

/// Ядро зовёт тулы модулей с M4; до того тул только объявлен в `hello`.
async fn say(ctx: ToolCtx, input: SayInput) -> Result<ToolResult, ToolError> {
    ctx.progress("echoing").await;
    Ok(ToolResult::ok(vec![ToolContent::text(input.text)]))
}

/// Сервис получает события только о чатах surface, которые создал сам.
async fn on_chat(_kernel: Kernel, event: Event) {
    if let Event::ChatCreated(chat) = event {
        log::info!(chat:% = chat.chat, surface:% = chat.surface; "chat created");
    }
}

fn main() -> eyre::Result<()> {
    ServiceConfig::register(App::default().env_prefix("ECHO_")).run(entrypoint)
}

async fn entrypoint(_: NoArgs, cfgs: ComponentConfigs) -> eyre::Result<()> {
    let cfg = cfgs.get::<ServiceConfig>();

    let listening = Module::new()
        .version(env!("CARGO_PKG_VERSION"))
        .about("echo example module")
        .group("echo", "Echo example")
        .tool("echo.say", say)
        .idempotent(true)
        .on_event(Topic::ChatCreated, on_chat)
        .listen(&cfg)
        .await?;

    // Ждёт первого дозвона ядра до `module.kernel_wait`.
    let mut kernel = listening.kernel().await?;
    log::info!(module:? = listening.module(), caller:? = kernel.caller(); "kernel connected");

    // Идемпотентно для создателя: безопасно звать на каждом старте.
    let surfaces::create::Ok { surface } = kernel
        .surfaces()
        .create()
        .call(surfaces::create::Args {
            id: SURFACE.into(),
            name: "Echo".into(),
            rules: None,
        })
        .await?;

    let kv::get::Ok { value } = kernel
        .kv()
        .get()
        .call(kv::get::Args {
            key: "starts".into(),
        })
        .await?;
    let starts = match value {
        Some(Value::Integer(n)) => n + 1,
        _ => 1,
    };
    kernel
        .kv()
        .set()
        .call(kv::set::Args {
            key: "starts".into(),
            value: Value::Integer(starts),
        })
        .await?;
    log::info!(surface:% = surface.id, starts; "ready");

    // Рестарт ядра не завершает `run`: ядро перезвонит, `kernel` последует за ним.
    listening
        .run(async {
            _ = tokio::signal::ctrl_c().await;
        })
        .await
}
