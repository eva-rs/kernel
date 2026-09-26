//! Блокнот чата.

use eva::{data, time::Timestamp};

#[data]
pub struct Note {
    pub name: String,
    pub description: String,
    pub body: String,
    /// Тело до последней правки — клиенты рисуют дифф.
    pub prev_body: Option<String>,
    pub pinned: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[data(copy, ord)]
pub enum Mode {
    Inline,
    Squeeze,
    Index,
}
