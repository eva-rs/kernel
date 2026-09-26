//! Типы протокола Евы: аргументы и результаты операций, ошибки, wire-формы.
//! Без I/O и без рантайма — крейт видят все слои, включая клиентов.
//!
//! Раскладка повторяется во всех слоях: домен `foo` живёт в
//! `requests/foo.rs`, его атомарные ошибки — в `errors/foo.rs`, сущности —
//! в `types/foo.rs`; кадры и методы ядро ↔ модуль — в `wire`.
//!
//! Кодировка провода одна: msgpack с именами полей
//! (`rmp_serde::to_vec_named`). Позиционные массивы ломают untagged
//! `Generic`, `#[serde(flatten)]` и пропуск незнакомых полей пира новее.

#[macro_use]
mod tolerant;
#[macro_use]
pub mod errors;
pub mod requests;
pub mod types;
pub mod wire;

#[doc(hidden)]
pub mod _priv {
    pub use eva::paste;
}
