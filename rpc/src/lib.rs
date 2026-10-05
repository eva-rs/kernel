//! Eva RPC (`06`, `11 §6`): msgpack-кадры поверх uds/ws/wss и JSON-RPC мост
//! поверх http/https у клиентов, Noise поверх uds/tcp у сервисов,
//! симметричный мультиплексированный пир, серверный диспетчер операций
//! библиотеки и клиентская сессия, реализующая те же трейты неймспейсов.
//!
//! Слои снизу вверх: `transport` даёт `Conn` (сырые кадры), `peer` — вызовы,
//! потоки, загрузки, отмены и ping поверх него, `server`/`client` — handshake
//! и операции `<namespace>.<op>`, `module` — обратное направление ядро → модуль.
//! Кто набрал соединение, не меняет ролей: `hello` шлёт модуль, `welcome` —
//! ядро (`Server::accept` у клиента, `Server::dial` у сервиса).

pub mod client;
pub mod codec;
pub mod config;
pub mod gate;
pub mod module;
pub mod peer;
pub mod server;
pub mod transport;

pub use client::{ClientOptions, ConnectError, RpcClient, RpcSession};
pub use config::{ClientTls, Config, Listen, ServiceListen};
pub use module::{ModuleClient, ModuleHandler, ModuleProvider, NoHandler, Progress};
pub use peer::{Call, IdSeq, Kind, Outgoing, Peer, RemoteStream, Service};
pub use server::{Accepted, Authenticator, DialError, Dispatcher, Origin, Server};
pub use transport::{
    Conn, Incoming, Listener, RawConn, ServiceListener, TransportError,
    activation::Inherited,
    noise::{KeyError, Psk},
    tls::install_crypto_provider,
};
