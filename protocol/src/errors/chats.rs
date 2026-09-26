use eva::data;

use crate::types::ChatId;

#[data(error, display("chat {chat} does not exist"))]
pub struct NotFound {
    pub chat: ChatId,
}

#[data(error, display("chat {chat} is undeletable"))]
pub struct Undeletable {
    pub chat: ChatId,
}

#[data(error, display("moving chat {chat} changes trust {from} -> {to}; repeat with confirm"))]
pub struct TrustChange {
    pub chat: ChatId,
    pub from: bool,
    pub to: bool,
}
