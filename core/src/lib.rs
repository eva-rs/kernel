//! API Евы как библиотека.
//!
//! Основной интерфейс soul — `Session`: неймспейс по домену, операция по
//! методу.
//!
//! ```ignore
//! let mut session = soul.make_session(Caller::Soul).await?;
//! let surfaces::get::Ok { surface } = session
//!     .surfaces()
//!     .get()
//!     .call(surfaces::get::Args { surface: "local".into() })
//!     .await?;
//! ```
//!
//! Транспорты (msgpack-RPC, тулы модели) — обвязка над этими же трейтами,
//! поэтому у кода в модуле и у кода внутри soul форма вызова одна.

pub mod caller;
pub mod prelude;
pub mod provider;
pub mod service;

pub use caller::Caller;
pub use eva_protocol as protocol;
pub use provider::{GenSink, Provider};
pub use service::{IsSession, NAMESPACES, Op, OpKind, Session, SessionMaker};

pub use service::for_each_op;
