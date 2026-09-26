//! `Session`, неймспейсы и шаг вызова.

use eva::{auto_impl, handling, trait_set};

use eva_protocol::{errors::Aux, requests::Response};

use crate::Caller;

/// Трейт неймспейса из списка операций.
///
/// Запись `method` разворачивается в
/// `fn method(&mut self) -> impl CallStep<module::Args, Ok = module::Ok, Err = module::Err>`
/// против указанного модуля операций; `method => module` задаёт другое имя
/// модуля.
macro_rules! service_trait {
    (
        $(#[$meta:meta])*
        pub trait $Trait:ident($($reqs:ident)::+) {
            $($body:tt)*
        }
    ) => {
        service_trait!(@parse [$(#[$meta])*] $Trait [$($reqs)::+] [] $($body)*);
    };

    (@parse $meta:tt $Trait:ident $reqs:tt [$($acc:tt)*] $method:ident => $module:ident, $($rest:tt)*) => {
        service_trait!(@parse $meta $Trait $reqs [$($acc)* [$method $module $reqs]] $($rest)*);
    };
    (@parse $meta:tt $Trait:ident $reqs:tt [$($acc:tt)*] $method:ident => $module:ident) => {
        service_trait!(@parse $meta $Trait $reqs [$($acc)* [$method $module $reqs]]);
    };
    (@parse $meta:tt $Trait:ident $reqs:tt [$($acc:tt)*] $method:ident, $($rest:tt)*) => {
        service_trait!(@parse $meta $Trait $reqs [$($acc)* [$method $method $reqs]] $($rest)*);
    };
    (@parse $meta:tt $Trait:ident $reqs:tt [$($acc:tt)*] $method:ident) => {
        service_trait!(@parse $meta $Trait $reqs [$($acc)* [$method $method $reqs]]);
    };

    (@parse [$($meta:tt)*] $Trait:ident $reqs:tt [$([$method:ident $module:ident [$($path:ident)::+]])*]) => {
        $($meta)*
        #[::eva::auto_impl(&mut, Box)]
        pub trait $Trait: Send + Sync {$(
            fn $method(
                &mut self,
            ) -> impl $crate::service::CallStep<
                $($path)::+::$module::Args,
                Ok = $($path)::+::$module::Ok,
                Err = $($path)::+::$module::Err,
            >;
        )*}
    };
}

/// Регистрирует неймспейсы сессии: сумма трейтов и проекции `Session`.
macro_rules! domains {
    ($($method:ident: $($Trait:ident)::+),* $(,)?) => {
        trait_set! {
            pub trait IsSession = Send + Sync $(+ $($Trait)::+)*;
        }

        impl<S: IsSession> Session<S> {$(
            pub const fn $method(&mut self) -> impl $($Trait)::+ {
                &mut self.session
            }
        )*}
    };
}

pub mod chats;
pub mod surfaces;

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

domains! {
    surfaces: surfaces::Surfaces,
    chats: chats::Chats,
}

/// Кто умеет выдать сессию: ядро, RPC-клиент, мок в тесте.
#[auto_impl(&, &mut, Arc)]
pub trait SessionMaker: Send + Sync {
    type Session: IsSession;

    fn make_session(&self, caller: Caller) -> impl AuxFut<Session<Self::Session>>;
}

/// Шаг вызова: `session.chats().get()` даёт его, `.call(args)` исполняет.
pub trait CallStep<I>: Send + Sync {
    type Ok;
    type Err;

    fn call(&mut self, args: I) -> impl RespFut<Self::Ok, Self::Err>;
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
