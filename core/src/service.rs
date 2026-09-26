//! `Session`, неймспейсы и шаги вызова.

use std::pin::Pin;

use eva::{auto_impl, bytes::Bytes, handling, trait_set};
use futures_core::Stream;

use eva_protocol::{errors::Aux, requests::Response};

use crate::Caller;

/// Трейт неймспейса из списка операций.
///
/// Метка перед именем задаёт форму шага: без метки — чтение
/// (`CallStep`), `mut` — мутация (`CallStep`, окно идемпотентности),
/// `stream` — поток к вызывающему (`StreamStep`), `upload` — поток от
/// вызывающего (`UploadStep`). `method => module` задаёт другое имя модуля
/// операций. Рядом с трейтом появляется `OPS` — описание операций для
/// диспетчера и тестов покрытия.
macro_rules! service_trait {
    (
        $(#[$meta:meta])*
        pub trait $Trait:ident($($reqs:ident)::+) {
            $($body:tt)*
        }
    ) => {
        service_trait!(@parse [$(#[$meta])*] $Trait [$($reqs)::+] [] $($body)*);
    };

    (@parse $meta:tt $Trait:ident $reqs:tt $acc:tt) => {
        service_trait!(@emit $meta $Trait $acc);
    };
    (@parse $meta:tt $Trait:ident $reqs:tt $acc:tt mut $($rest:tt)*) => {
        service_trait!(@op $meta $Trait $reqs $acc [CallStep Ok Mutating] $($rest)*);
    };
    (@parse $meta:tt $Trait:ident $reqs:tt $acc:tt stream $method:ident $($rest:tt)*) => {
        service_trait!(@op $meta $Trait $reqs $acc [StreamStep Item Stream] $method $($rest)*);
    };
    (@parse $meta:tt $Trait:ident $reqs:tt $acc:tt upload $method:ident $($rest:tt)*) => {
        service_trait!(@op $meta $Trait $reqs $acc [UploadStep Ok Upload] $method $($rest)*);
    };
    (@parse $meta:tt $Trait:ident $reqs:tt $acc:tt $method:ident $($rest:tt)*) => {
        service_trait!(@op $meta $Trait $reqs $acc [CallStep Ok Read] $method $($rest)*);
    };

    (@op $meta:tt $Trait:ident $reqs:tt [$($acc:tt)*] $shape:tt $method:ident => $module:ident $(, $($rest:tt)*)?) => {
        service_trait!(@parse $meta $Trait $reqs [$($acc)* [$method $module $reqs $shape]] $($($rest)*)?);
    };
    (@op $meta:tt $Trait:ident $reqs:tt [$($acc:tt)*] $shape:tt $method:ident $(, $($rest:tt)*)?) => {
        service_trait!(@parse $meta $Trait $reqs [$($acc)* [$method $method $reqs $shape]] $($($rest)*)?);
    };

    (@emit [$($meta:tt)*] $Trait:ident [$([
        $method:ident $module:ident [$($path:ident)::+] [$Step:ident $Assoc:ident $kind:ident]
    ])*]) => {
        $($meta)*
        #[::eva::auto_impl(&mut, Box)]
        pub trait $Trait: Send + Sync {$(
            fn $method(
                &mut self,
            ) -> impl $crate::service::$Step<
                $($path)::+::$module::Args,
                $Assoc = $($path)::+::$module::$Assoc,
                Err = $($path)::+::$module::Err,
            >;
        )*}

        /// Операции неймспейса в порядке объявления.
        pub const OPS: &[$crate::service::Op] = &[$(
            $crate::service::Op {
                name: $crate::service::op_name(stringify!($module)),
                kind: $crate::service::OpKind::$kind,
            },
        )*];
    };
}

/// Регистрирует неймспейсы сессии: сумма трейтов, проекции `Session` и
/// `NAMESPACES`. Имя метода сессии совпадает с модулем `service/<name>.rs`.
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

        /// Все операции библиотеки: RPC-метод — `<namespace>.<op>`.
        pub const NAMESPACES: &[(&str, &[Op])] = &[$((stringify!($method), $method::OPS)),*];
    };
}

pub mod agents;
pub mod blobs;
pub mod chats;
pub mod cron;
pub mod folders;
pub mod fs;
pub mod hands_grants;
pub mod inference;
pub mod kv;
pub mod memory;
pub mod models;
pub mod modules;
pub mod notes;
pub mod persons;
pub mod sleep;
pub mod sqlite;
pub mod stats;
pub mod surfaces;
pub mod tools;
pub mod turns;

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
    folders: folders::Folders,
    chats: chats::Chats,
    turns: turns::Turns,
    persons: persons::Persons,
    memory: memory::Memory,
    notes: notes::Notes,
    tools: tools::Tools,
    models: models::Models,
    cron: cron::Cron,
    sleep: sleep::Sleep,
    agents: agents::Agents,
    stats: stats::Stats,
    modules: modules::Modules,
    hands_grants: hands_grants::HandsGrants,
    blobs: blobs::Blobs,
    inference: inference::Inference,
    fs: fs::Fs,
    sqlite: sqlite::Sqlite,
    kv: kv::Kv,
}

/// Форма шага операции.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpKind {
    Read,
    /// Повтор того же запроса в окне идемпотентности отдаёт сохранённый ответ.
    Mutating,
    Stream,
    Upload,
}

/// Описание операции: имя модуля операций и форма шага.
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

/// Шаг вызова: `session.chats().get()` даёт его, `.call(args)` исполняет.
pub trait CallStep<I>: Send + Sync {
    type Ok;
    type Err;

    fn call(&mut self, args: I) -> impl RespFut<Self::Ok, Self::Err>;
}

/// Элементы потока; `Err` внутри потока — последний элемент.
pub type Items<T, E> = Pin<Box<dyn Stream<Item = Response<T, E>> + Send>>;

/// Шаг потока к вызывающему: отказ до первого элемента — `Err` из `open`,
/// drop `Items` — отмена.
pub trait StreamStep<I>: Send + Sync {
    type Item;
    type Err;

    fn open(&mut self, args: I) -> impl RespFut<Items<Self::Item, Self::Err>, Self::Err>;
}

/// Тело загрузки: чанки от вызывающего; обрыв транспорта — `Aux`.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, Aux>> + Send>>;

/// Шаг потока от вызывающего: `Args` описывают тело, `body` его несёт.
pub trait UploadStep<I>: Send + Sync {
    type Ok;
    type Err;

    fn call(&mut self, args: I, body: ByteStream) -> impl RespFut<Self::Ok, Self::Err>;
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
