//! Eva SDK (`06 §10`): модуль ядра поверх `eva-rpc` — handshake, смена
//! соединения, обработчики вызовов ядро → модуль.
//!
//! Сервис (мост мессенджера, MCP) адреса ядра не знает: он слушает, ядро
//! набирает его само по записи `modules.services.<id>` (Noise с общим ключом).
//!
//! ```ignore
//! let listening = Module::new()
//!     .about("Messenger X surface")
//!     .ask(true)
//!     .listen(&cfg) // ServiceConfig: module.listen, module.key
//!     .await?;
//! // Идемпотентно для создателя: безопасно на каждом старте, `name`/`rules` меняет `surfaces.update`.
//! listening
//!     .kernel()
//!     .await?
//!     .surfaces()
//!     .create()
//!     .call(surfaces::create::Args {
//!         id: "x".into(),
//!         name: "Messenger X".into(),
//!         rules: Some(RULES.into()),
//!     })
//!     .await?;
//! listening.run(shutdown).await
//! ```
//!
//! Клиент хозяина (телефон, TUI) звонит ядру сам с токеном:
//! `Module::connect_config(&ClientConfig)` → `Connected` с реконнектом.
//!
//! Имени у модуля нет: id — ключ записи реестра (сервис) или токена (клиент)
//! у ядра, приходит в `welcome`. Surface — сущность ядра: модуль её только
//! создаёт, обслуживание ядро выводит из живой сессии, поэтому после смены
//! соединения регистрировать нечего.
//!
//! `Kernel` — та же `Session`, что внутри ядра: форма вызова одна (`11 §6`);
//! ручка следует за сменой соединения, её можно держать всё время жизни модуля.
//!
//! Что работает в M1: приём ядра и подключение к нему, `hello.resume`,
//! события (ядро уже рассылает `chat.*`/`module.*` по
//! `Policy::events().deliver`), запасной вход `.provider()`. Билдеры
//! `.tool()`/`.hook()` регистрируют обработчики и capability сейчас, но ядро
//! лишь хранит их: тулы оно зовёт с M4, хуки — с M2. Реплей `turns.attach`
//! после смены соединения — M2 (`module::replay_attach`).

pub mod backoff;
pub mod config;
pub mod handler;
pub mod listen;
pub mod live;
pub mod module;
pub mod tool;

/// Шаги и трейты неймспейсов: `use eva_sdk::prelude::*`.
pub mod prelude {
    pub use eva_core::prelude::*;

    pub use crate::{
        Kernel,
        listen::Listening,
        module::{Connected, Module},
        tool::{ToolCtx, ToolError},
    };
}

pub use eva::utils::SecretString;
pub use eva_core::{Caller, Provider};
pub use eva_protocol::{
    self as protocol,
    types::{
        module::ServiceAddress,
        tool::{Need, ToolContent},
    },
    wire::{
        capabilities::FailMode,
        events::{Event, Publish, Topic},
        hooks::{AfterTool, BeforeTool, BeforeTurn, Decision, HookPoint, NoPatch},
        tools::ToolResult,
    },
};
pub use eva_rpc::{ClientOptions, ClientTls, ConnectError, RpcClient, ServiceListen};

pub use self::{
    backoff::Backoff,
    config::{ClientConfig, ServiceConfig},
    handler::HookInput,
    listen::Listening,
    module::{Connected, Module},
    tool::{ToolCtx, ToolError},
};

/// Клиент ядра: все неймспейсы операций; каждый вызов идёт через текущее
/// соединение модуля и переживает его смену (`live`).
pub type Kernel = eva_core::Session<live::LiveSession>;
