//! `Session`, неймспейсы и шаги вызова.
//!
//! Трейты неймспейсов (`service::<ns>::<Ns>`) и X-макро `for_each_op!`
//! порождены списком операций в `eva-kernel-macros`; шаг операции —
//! `CallStep<Args, Output = Response<Ok, Err>>`.

use eva::{auto_impl, handling, trait_set};

use eva_protocol::{errors::Aux, requests::Response};

use crate::Caller;

pub use eva::handling::CallStep;

eva_kernel_macros::endpoints!(for_each_op);

/// Из списка операций: `IsSession`, проекции `Session`, `traits` для `prelude`
/// и `NAMESPACES`.
macro_rules! domains {
    ($(
        $segment:ident {$(
            $accessor:ident : $Trait:ident {$(
                $(#[$flag:ident])*
                $method:ident = $endpoint:ident {
                    args: $Args:ty,
                    ok: $Ok:ty,
                    err: $Err:ty,
                    output: $Output:ty,
                }
            )*}
        )*}
    )*) => {
        trait_set! {
            pub trait IsSession = Send + Sync $($(+ $segment::$Trait)*)*;
        }

        impl<S: IsSession> Session<S> {$($(
            pub const fn $accessor(&mut self) -> impl $segment::$Trait {
                &mut self.session
            }
        )*)*}

        /// Все трейты неймспейсов анонимно: glob-импорт открывает методы `Session`.
        pub mod traits {
            $($(pub use super::$segment::$Trait as _;)*)*
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
    pub trait RespFut<O, E> = Future<Output = Response<O, E>> + Send;
    pub trait AuxFut<O> = Future<Output = Result<O, Aux>> + Send;

    pub trait IsService = SessionMaker;
}

pub type SessionOf<T> = <T as SessionMaker>::Session;

/// Сессия: неймспейсы плюс тот, кто зовёт.
#[derive(Debug, Clone)]
pub struct Session<S> {
    session: S,
    caller: Caller,
}

impl<S: IsSession> Session<S> {
    pub const fn new(session: S, caller: Caller) -> Self {
        Self { session, caller }
    }

    pub const fn caller(&self) -> &Caller {
        &self.caller
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
