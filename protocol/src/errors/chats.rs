use eva::data;

use crate::types::{ChatId, EventId, SurfaceId, chat::Selector};

#[data(error, display("chat {chat} does not exist"))]
pub struct NotFound {
    pub chat: Selector,
}

code!(NotFound => NotFound);

#[data(error, display("chat {chat} is undeletable"))]
pub struct Undeletable {
    pub chat: ChatId,
}

code!(Undeletable => Conflict);

#[data(
    error,
    display("moving chat {chat} changes trust {from} -> {to}; repeat with confirm")
)]
pub struct TrustChange {
    pub chat: ChatId,
    pub from: bool,
    pub to: bool,
}

code!(TrustChange => Conflict);

#[data(
    error,
    display("chat {chat} lives on surface {surface}: folders of other surfaces are out of reach")
)]
pub struct CrossSurface {
    pub chat: ChatId,
    pub surface: SurfaceId,
}

code!(CrossSurface => InvalidParams);

#[data(error, display("chat {chat} is not deleted"))]
pub struct NotDeleted {
    pub chat: ChatId,
}

code!(NotDeleted => Conflict);

#[data(
    error,
    display("chat {chat} is persistent: kind only goes ephemeral -> persistent")
)]
pub struct KindMonotonic {
    pub chat: ChatId,
}

code!(KindMonotonic => Conflict);

#[data(
    error,
    display("chat {chat} is a subagent: its trust is a snapshot of the lead")
)]
pub struct AgentTrust {
    pub chat: ChatId,
}

code!(AgentTrust => Conflict);

#[data(error, display("message {message} does not exist in chat {chat}"))]
pub struct MessageNotFound {
    pub chat: ChatId,
    pub message: EventId,
}

code!(MessageNotFound => NotFound);

#[data(
    error,
    display("message {message} is already delivered as {external_id}")
)]
pub struct AlreadyDelivered {
    pub message: EventId,
    pub external_id: String,
}

code!(AlreadyDelivered => Conflict);
