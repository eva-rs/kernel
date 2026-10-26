//! Высокоуровневый SDK модулей Eva: группы тулов, типизированное состояние,
//! конфиг приложения и локализованный прогресс поверх симметричного RPC.
//!
//! Каноничный вход — `module!()`: имя и версия берутся из Cargo вызывающего
//! пакета, `.name()` и `.version()` переопределяют их. Обработчики объявляются
//! внутри `Group` через `Tool::new`; их имена локальны относительно группы.
//! `describe(Description { … })` задаёт настройки тула целиком; отдельные
//! сеттеры меняют одно поле. По умолчанию `idempotent = parallel_safe = true`.
//!
//! `Module::run` загружает конфиг, выполняет `init`, ждёт звонков от soul и управляет
//! остановкой через `eva::supervisor::Supervisor`. По умолчанию сокет —
//! `$RUNTIME_DIRECTORY/module.sock` (иначе `/run/eva-<name>/module.sock`),
//! ключ — `SecretString::credential("soul-key")`.
//!
//! Для встраивания доступны `listen(&ServiceConfig)` → `Listening` и
//! `connect_config(&ClientConfig)` → `Connected`. Авторизованный id модуля
//! приходит от soul в `welcome`. `Soul = Session<LiveSession>` переживает
//! смену соединения и предоставляет общее библиотечное API soul.
//!
//! `msg!` создаёт сообщение с именованными параметрами; `include_translations!`
//! встраивает PO-каталоги. `ToolCtx::progress` отправляет fallback и переводы
//! текущей строки, а язык выбирает каждый получатель ленты.

pub mod app;
pub mod backoff;
pub mod config;
pub mod group;
pub mod handler;
pub mod listen;
pub mod live;
pub mod module;
pub mod tool;

/// Сессии и трейты неймспейсов: `use eva_sdk::prelude::*`.
pub mod prelude {
    pub use eva_core::prelude::*;

    pub use crate::{
        Description, Group, ResultForm, Soul, Tool,
        listen::Listening,
        module::{self, Connected, Module},
        msg,
        tool::{ToolCtx, ToolError},
    };
}

pub use eva::utils::SecretString;
pub use eva_protocol::types::text::{Catalog, Catalogs, LocalizedText, Message};
pub use eva_sdk_macros::{include_translations, msg};

#[doc(hidden)]
pub mod __private {
    pub fn catalogs(serialized: &str) -> crate::Catalogs {
        serde_json::from_str(serialized).expect("catalogs were validated by include_translations")
    }
}
pub use eva_core::{Caller, Provider};
pub use eva_protocol::{
    self as protocol,
    types::{
        module::ServiceAddress,
        tool::{Need, ResultForm, ToolContent},
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
    app::ServiceDefaults,
    backoff::Backoff,
    config::{ClientConfig, ServiceConfig},
    group::{Description, Group, IntoToolResult, Tool},
    handler::HookInput,
    listen::Listening,
    module::{Connected, Module},
    tool::{ToolCtx, ToolError},
};

/// Клиент soul: все неймспейсы операций; каждый вызов идёт через текущее
/// соединение модуля и переживает его смену (`live`).
pub type Soul = eva_core::Session<live::LiveSession>;

/// Метаданные берутся из пакета, который вызывает макрос.
#[macro_export]
macro_rules! module {
    () => {
        $crate::Module::new()
            .name(env!("CARGO_PKG_NAME"))
            .version(env!("CARGO_PKG_VERSION"))
    };
}
