//! Библиотека ядра: `Kernel<C>: SessionMaker` и всё под ним (`docs/design/11 §5`).
//!
//! Раскладка: `handling/` — хендлер на операцию, `impls/` — namespace-трейты
//! над ними, `state/policy/` — авторизация, `db/` — весь SQL, `convert/` —
//! строки БД в типы протокола, `subsystems/` — компоненты `Supervisor`.

pub mod bus;
pub mod components;
pub mod config;
pub mod convert;
pub mod db;
pub mod handling;
pub mod hands;
mod impls;
pub mod kernel;
pub mod markers;
pub mod providers;
pub mod registry;
pub mod session;
pub mod shared;
pub mod state;
pub mod subsystems;
pub mod world;

pub use self::{
    components::{Components, Real},
    config::Config,
    kernel::{Kernel, Parts},
    session::Session,
};

/// Версия ядра для `welcome` и логов: крейт и ревизия сборки, если её
/// передала сборка (`EVA_COMMIT`).
pub fn version() -> String {
    match option_env!("EVA_COMMIT") {
        Some(commit) => format!("{}+{commit}", env!("CARGO_PKG_VERSION")),
        None => env!("CARGO_PKG_VERSION").to_owned(),
    }
}
