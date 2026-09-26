//! Постоянные разрешения рук.

use eva::{data, time::Timestamp};

use crate::types::{ChatId, HandsGrantId, PersonId, SurfaceId};

#[data]
pub struct Grant {
    pub id: HandsGrantId,
    /// Имя из `hands.endpoints`: согласие на одной машине не покрывает другую.
    pub endpoint: String,
    pub surface: SurfaceId,
    /// `None` — все чаты surface.
    pub chat: Option<ChatId>,
    pub kind: GrantKind,
    /// Префикс пути или команды.
    pub prefix: String,
    pub issued_by: PersonId,
    pub created_at: Timestamp,
}

#[data(copy, ord)]
pub enum GrantKind {
    Read,
    Write,
    Shell,
}
