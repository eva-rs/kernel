//! Модули ядра.

use eva::{data, time::Timestamp};

use crate::types::ModuleId;

/// Единственный бит прав токена: `Caller::Client` либо `Caller::Service`.
#[data(copy, ord, display(name))]
pub enum TokenKind {
    Client,
    Service,
}

#[data(copy, ord)]
pub enum State {
    Connected,
    Offline,
    Forgotten,
}

#[data]
pub struct Module {
    pub name: ModuleId,
    /// `None` — живого токена нет.
    pub kind: Option<TokenKind>,
    pub state: State,
    pub version: String,
    pub proto_version: String,
    /// Последний переход состояния.
    pub since: Timestamp,
    /// Объявленные capability: `provider`, `tools`, `events`, `hooks`, `ask`.
    pub capabilities: Vec<String>,
}
