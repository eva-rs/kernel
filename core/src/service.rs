//! `Session`, неймспейсы и шаги вызова.
//!
//! `#[service]` объявляет трейты неймспейсов в протоколе; `#[export]`
//! объединяет их в X-макро `for_each_op!`. Шаг операции —
//! `CallStep<Args, Output = Response<Ok, Err>>`.

use eva::{auto_impl, export, handling, trait_set};

use eva_protocol::{errors::Aux, requests::Response};

use crate::{
    Caller,
    protocol::requests::{
        Agents, Blobs, Chats, Cron, Folders, Fs, HandsGrants, Inference, Kv, Memory, Models,
        Modules, Notes, Persons, Sleep, Sqlite, Stats, Surfaces, Tools, Turns,
    },
};

#[export(name = for_each_op)]
#[derive(Debug, Clone)]
pub struct Services {
    surfaces: Surfaces,
    folders: Folders,
    chats: Chats,
    turns: Turns,
    persons: Persons,
    memory: Memory,
    notes: Notes,
    tools: Tools,
    models: Models,
    cron: Cron,
    sleep: Sleep,
    agents: Agents,
    stats: Stats,
    modules: Modules,
    hands_grants: HandsGrants,
    blobs: Blobs,
    inference: Inference,
    fs: Fs,
    sqlite: Sqlite,
    kv: Kv,
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

/// Кто умеет выдать сессию: soul, RPC-клиент, мок в тесте.
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
