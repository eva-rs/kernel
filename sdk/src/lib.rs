//! Eva SDK (`06 §10`): модуль ядра поверх `eva-rpc` — handshake, реконнект,
//! обработчики вызовов ядро → модуль.
//!
//! ```ignore
//! let connected = Module::new()
//!     .about("Messenger X surface")
//!     .ask(true)
//!     .connect(&cfg.kernel, &cfg.token)
//!     .await?;
//! // Идемпотентно для создателя: безопасно на каждом старте, `name`/`rules` меняет `surfaces.update`.
//! connected
//!     .kernel()
//!     .surfaces()
//!     .create()
//!     .call(surfaces::create::Args {
//!         id: "x".into(),
//!         name: "Messenger X".into(),
//!         rules: Some(RULES.into()),
//!     })
//!     .await?;
//! connected.run(shutdown).await
//! ```
//!
//! Имени у модуля нет: id — имя записи его токена у ядра, приходит в
//! `welcome` (`Connected::module`). Surface — сущность ядра: модуль её
//! только создаёт, обслуживание ядро выводит из живой сессии, поэтому после
//! реконнекта регистрировать нечего.
//!
//! `Kernel` — та же `Session`, что внутри ядра: форма вызова одна (`11 §6`);
//! ручка следует за реконнектом, её можно держать всё время жизни модуля.
//!
//! Что работает в M1: подключение и ожидание ядра, реконнект с
//! `hello.resume`, события (ядро уже рассылает `chat.*`/`module.*` по
//! `Policy::events().deliver`), запасной вход `.provider()`. Билдеры
//! `.tool()`/`.hook()` регистрируют обработчики и capability сейчас, но ядро
//! лишь хранит их: тулы оно зовёт с M4, хуки — с M2. Реплей `turns.attach`
//! после реконнекта — M2 (`module::replay_attach`).

pub mod backoff;
pub mod config;
pub mod handler;
pub mod live;
pub mod module;
pub mod tool;

/// Шаги и трейты неймспейсов: `use eva_sdk::prelude::*`.
pub mod prelude {
    pub use eva_core::prelude::*;

    pub use crate::{
        Kernel,
        module::{Connected, Module},
        tool::{ToolCtx, ToolError},
    };
}

pub use eva::utils::SecretString;
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
    config::ModuleConfig,
    handler::HookInput,
    module::{Connected, Module},
    tool::{ToolCtx, ToolError},
};

/// Клиент ядра: все неймспейсы операций; каждый вызов идёт через текущее
/// соединение модуля и переживает реконнект (`live`).
pub type Kernel = eva_core::Session<live::LiveSession>;
