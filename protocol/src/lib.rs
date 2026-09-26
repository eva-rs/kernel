//! Типы протокола Евы: аргументы и результаты операций, ошибки, wire-формы.
//! Без I/O и без рантайма — крейт видят все слои, включая клиентов.
//!
//! Раскладка повторяется во всех слоях: домен `foo` живёт в
//! `requests/foo.rs`, его атомарные ошибки — в `errors/foo.rs`, сущности —
//! в `types/foo.rs`.

pub mod errors;
pub mod requests;
pub mod types;

#[doc(hidden)]
pub mod _priv {
    pub use eva::paste;
}
