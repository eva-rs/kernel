//! Проводные типы `06`: кадры, handshake, capabilities и методы soul → модуль.
//!
//! Кадр несёт payload непрозрачными байтами — msgpack `Args`/`Ok`/`Item`
//! операции: разбор в два шага держит `Frame` без динамического `Value`, а
//! незнакомый payload роняет только свой вызов, не соединение.
//!
//! Совместимость MINOR (`06 §8`): enum'ы, которые пир принимает, терпят
//! незнакомый вариант (`Unknown` или ближайший общий вариант,
//! `crate::tolerant`); незнакомые поля структур serde пропускает сам.
//! Аргументы запросов строгие: получатель обязан понять их целиком, иначе
//! `invalid_params`.

pub mod capabilities;
pub mod events;
pub mod frame;
pub mod handshake;
pub mod hooks;
pub mod provider;
pub mod tools;

pub use capabilities::Capabilities;
pub use frame::{ErrorCode, Frame, RpcError};
pub use handshake::{Hello, Welcome};

/// Методы вне операций библиотеки; хуки — `HookPoint::method`.
pub mod method {
    /// Модуль → soul, первый кадр сессии: `Hello` → `Welcome`.
    pub const HELLO: &str = "hello";
    /// `notify` в обе стороны, params — `Ping {}`.
    pub const PING: &str = "ping";
    /// `notify` в обе стороны, params — `RpcError`: причина закрытия, последний
    /// кадр перед закрытием сокета (uds закрывается без причины).
    pub const CLOSE: &str = "close";
    /// Soul → модуль: `ToolCall` → поток `ToolItem`.
    pub const TOOLS_CALL: &str = "tools.call";
    /// Soul → модуль, `notify`: `Publish`.
    pub const EVENTS_PUBLISH: &str = "events.publish";
    /// Soul → модуль-провайдер: `GenerateRequest` → поток `GenItem`.
    pub const PROVIDER_GENERATE: &str = "provider.generate";
    /// `EmbedRequest` → `EmbedResponse`.
    pub const PROVIDER_EMBED: &str = "provider.embed";
    /// `GenerateRequest` → `TokenCount`.
    pub const PROVIDER_COUNT_TOKENS: &str = "provider.count_tokens";
}
