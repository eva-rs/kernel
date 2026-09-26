//! Eva SDK (`06 §10`): модуль ядра поверх `eva-rpc` — handshake, surface,
//! реконнект, обработчики вызовов ядро → модуль.
//!
//! ```ignore
//! let connected = Module::new("messenger-x")
//!     .surface(Surface::new("x").name("Messenger X").rules(RULES).ask(true))
//!     .connect(&cfg.kernel, cfg.token.expose())
//!     .await?;
//! let mut kernel = connected.kernel();
//! kernel.kv().get().call(kv::get::Args { key: "offset".into() }).await?;
//! connected.run(shutdown).await
//! ```
//!
//! `Kernel` — та же `Session`, что внутри ядра: форма вызова одна (`11 §6`).
//!
//! Что работает в M1: подключение и ожидание ядра, `.surface()` после
//! каждого `welcome`, реконнект с `hello.resume`, события (ядро уже
//! рассылает `chat.*`/`module.*` по `Policy::events().deliver`), запасной
//! вход `.provider()`. Билдеры `.tool()`/`.hook()` регистрируют обработчики и
//! capability сейчас, но ядро лишь хранит их: тулы оно зовёт с M4, хуки — с
//! M2. Реплей `turns.attach` после реконнекта — M2 (`module::replay_attach`).

pub mod backoff;
pub mod config;
pub mod handler;
pub mod module;
pub mod surface;
pub mod tool;

/// Шаги и трейты неймспейсов: `use eva_sdk::prelude::*`.
pub mod prelude {
    pub use eva_core::prelude::*;

    pub use crate::{
        Kernel,
        module::{Connected, Module},
        surface::Surface,
        tool::{ToolCtx, ToolError},
    };
}

pub use eva_core::{Caller, Provider};
pub use eva_protocol::{
    self as protocol,
    types::tool::{Need, ToolContent},
    wire::{
        capabilities::FailMode,
        events::{Event, Publish, Topic},
        hooks::{AfterTool, BeforeSend, BeforeTool, BeforeTurn, Decision, HookPoint},
        tools::ToolResult,
    },
};
pub use eva_rpc::{ClientOptions, ClientTls, ConnectError, RpcClient};

pub use self::{
    backoff::Backoff,
    config::{ModuleConfig, Secret},
    handler::HookInput,
    module::{Connected, Module},
    surface::Surface,
    tool::{ToolCtx, ToolError},
};

/// Клиент ядра: все неймспейсы операций поверх текущего соединения.
pub type Kernel = eva_core::Session<eva_rpc::RpcSession>;
