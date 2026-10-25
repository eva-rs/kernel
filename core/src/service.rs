//! `Session`, неймспейсы и шаги вызова.
//!
//! `#[service]` объявляет трейты неймспейсов в протоколе; `#[export]`
//! объединяет их в X-макро `for_each_op!`. Шаг операции —
//! `CallStep<Args, Output = Response<Ok, Err>>`.

use eva::{auto_impl, export, handling, trait_set};

use eva_protocol::{errors::Aux, requests::Response};

use crate::Caller;

pub use eva::handling::CallStep;

pub use crate::protocol::requests::agents::Agents;
pub use crate::protocol::requests::blobs::Blobs;
pub use crate::protocol::requests::chats::Chats;
pub use crate::protocol::requests::cron::Cron;
pub use crate::protocol::requests::folders::Folders;
pub use crate::protocol::requests::fs::Fs;
pub use crate::protocol::requests::hands_grants::HandsGrants;
pub use crate::protocol::requests::inference::Inference;
pub use crate::protocol::requests::kv::Kv;
pub use crate::protocol::requests::memory::Memory;
pub use crate::protocol::requests::models::Models;
pub use crate::protocol::requests::modules::Modules;
pub use crate::protocol::requests::notes::Notes;
pub use crate::protocol::requests::persons::Persons;
pub use crate::protocol::requests::sleep::Sleep;
pub use crate::protocol::requests::sqlite::Sqlite;
pub use crate::protocol::requests::stats::Stats;
pub use crate::protocol::requests::surfaces::Surfaces;
pub use crate::protocol::requests::tools::Tools;
pub use crate::protocol::requests::turns::Turns;
#[export(name = for_each_op)]
#[derive(Debug, Clone)]
pub struct Services {
    surfaces: crate::protocol::requests::surfaces::Surfaces,
    folders: crate::protocol::requests::folders::Folders,
    chats: crate::protocol::requests::chats::Chats,
    turns: crate::protocol::requests::turns::Turns,
    persons: crate::protocol::requests::persons::Persons,
    memory: crate::protocol::requests::memory::Memory,
    notes: crate::protocol::requests::notes::Notes,
    tools: crate::protocol::requests::tools::Tools,
    models: crate::protocol::requests::models::Models,
    cron: crate::protocol::requests::cron::Cron,
    sleep: crate::protocol::requests::sleep::Sleep,
    agents: crate::protocol::requests::agents::Agents,
    stats: crate::protocol::requests::stats::Stats,
    modules: crate::protocol::requests::modules::Modules,
    hands_grants: crate::protocol::requests::hands_grants::HandsGrants,
    blobs: crate::protocol::requests::blobs::Blobs,
    inference: crate::protocol::requests::inference::Inference,
    fs: crate::protocol::requests::fs::Fs,
    sqlite: crate::protocol::requests::sqlite::Sqlite,
    kv: crate::protocol::requests::kv::Kv,
}
pub use ServicesFeatures as IsSession;

/// Из метаданных сервисов: `traits` для `prelude` и `NAMESPACES`.
macro_rules! domains {
    ($(
        $segment:ident {$(
            $accessor:ident : $Trait:ident {
                trait: $TraitPath:path,
                client: $Client:path,
                $(
                $(#[$flag:ident])*
                $method:ident = $endpoint:ident {
                    args: $Args:ty,
                    ok: $Ok:ty,
                    err: $Err:ty,
                    output: $Output:ty,
                    docs: [$($doc:literal),*],
                }
            )*}
        )*}
    )*) => {
        /// Все трейты неймспейсов анонимно: glob-импорт открывает методы `Session`.
        pub mod traits {
            $($(pub use $TraitPath as _;)*)*
        }

        /// Все операции библиотеки: RPC-метод — `<namespace>.<op>`.
        pub const NAMESPACES: &[(&str, &[Op])] = &[$(
            (stringify!($segment), &[$($(
                Op {
                    name: op_name(stringify!($endpoint)),
                    kind: op_kind!($($flag)*),
                },
            )*)*]),
        )*];
    };
}

macro_rules! op_kind {
    () => {
        OpKind::Read
    };
    (mutating) => {
        OpKind::Mutating
    };
    (stream) => {
        OpKind::Stream
    };
    (upload) => {
        OpKind::Upload
    };
}

for_each_op!(domains);

trait_set! {
    pub trait AuxFut<O> = Future<Output = Result<O, Aux>> + Send;

    pub trait IsService = SessionMaker;
}

pub type SessionOf<T> = <T as SessionMaker>::Session;

/// Сессия: неймспейсы плюс тот, кто зовёт.
#[derive(Debug, Clone)]
pub struct Session<S> {
    session: Services<S>,
    caller: Caller,
}

impl<S: IsSession> Session<S> {
    pub const fn new(session: S, caller: Caller) -> Self {
        Self {
            session: Services::new(session),
            caller,
        }
    }

    pub const fn caller(&self) -> &Caller {
        &self.caller
    }
}

impl<S> std::ops::Deref for Session<S> {
    type Target = Services<S>;
    fn deref(&self) -> &Self::Target {
        &self.session
    }
}
impl<S> std::ops::DerefMut for Session<S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.session
    }
}

/// Форма операции — флаг в списке операций.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpKind {
    Read,
    /// Повтор того же запроса в окне идемпотентности отдаёт сохранённый ответ.
    Mutating,
    /// `Ok` — `Items`: поток к вызывающему.
    Stream,
    /// `Args` несёт `ByteStream`: поток от вызывающего.
    Upload,
}

/// Описание операции: имя модуля операций и форма.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Op {
    pub name: &'static str,
    pub kind: OpKind,
}

/// `stringify!(r#move)` даёт `r#move`, а RPC-метод — `chats.move`.
#[doc(hidden)]
pub const fn op_name(name: &'static str) -> &'static str {
    match name.as_bytes() {
        [b'r', b'#', rest @ ..] => match str::from_utf8(rest) {
            Ok(name) => name,
            Err(_) => panic!("operation name is not utf-8"),
        },
        _ => name,
    }
}

/// Кто умеет выдать сессию: ядро, RPC-клиент, мок в тесте.
#[auto_impl(&, &mut, Arc)]
pub trait SessionMaker: Send + Sync {
    type Session: IsSession;

    fn make_session(&self, caller: Caller) -> impl AuxFut<Session<Self::Session>>;
}

/// Хендлер, чей `Output` — ответ операции.
pub trait IsEndpoint<I, S>:
    handling::Endpoint<I, S, Output = Response<Self::Ok, Self::Err>>
{
    type Ok;
    type Err;
}

impl<I, S, O, E, Ep> IsEndpoint<I, S> for Ep
where
    Ep: handling::Endpoint<I, S, Output = Response<O, E>>,
{
    type Ok = O;
    type Err = E;
}
