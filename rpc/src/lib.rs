//! Eva RPC (`06`, `11 §6`): msgpack-кадры поверх uds/ws/wss, симметричный
//! мультиплексированный пир, серверный диспетчер операций библиотеки и
//! клиентская сессия, реализующая те же трейты неймспейсов.
//!
//! Слои снизу вверх: `transport` даёт `Conn` (сырые кадры), `peer` — вызовы,
//! потоки, загрузки, отмены и ping поверх него, `server`/`client` — handshake
//! и операции `<namespace>.<op>`, `module` — обратное направление ядро → модуль.

pub mod client;
pub mod codec;
pub mod config;
pub mod module;
pub mod peer;
pub mod server;
pub mod transport;

mod ops;

pub use client::{ClientOptions, ConnectError, RpcClient, RpcSession};
pub use config::{ClientTls, Config, Listen};
pub use module::{ModuleClient, ModuleHandler, ModuleProvider, NoHandler, Progress};
pub use peer::{Call, IdSeq, Kind, Outgoing, Peer, RemoteStream, Service};
pub use server::{Accepted, Authenticator, Dispatcher, Server};
pub use transport::{Conn, Listener, RawConn, TransportError, tls::install_crypto_provider};
