//! Личность и её адреса на поверхностях.

use eva::{data, time::Timestamp};

use crate::types::{PersonId, SurfaceId};

#[data]
pub struct Person {
    pub id: PersonId,
    pub name: String,
    /// Положительный факт; из провода не выводится никогда.
    pub master: bool,
    /// Эффективно `whitelisted ∨ master`.
    pub whitelisted: bool,
    pub identities: Vec<Identity>,
    pub created_at: Timestamp,
}

#[data]
pub struct Identity {
    pub surface: SurfaceId,
    pub external_id: String,
    pub handle: Option<String>,
    pub name: Option<String>,
    pub first_seen: Timestamp,
}
