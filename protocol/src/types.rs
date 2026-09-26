//! Сущности протокола. Идентификаторы — `ids`, домены — по файлу на домен.

pub mod effort;
pub mod ids;

pub mod chat;
pub mod surface;

pub use effort::Effort;
pub use ids::*;
