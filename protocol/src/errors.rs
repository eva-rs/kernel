//! Модель ошибок: у каждой операции свой `Err`, а общие беды системы —
//! `Aux`. На проводе и в библиотеке это один тип `Generic<E>`, поэтому
//! вызывающий различает «операция честно отказала» и «подсистема упала».
//!
//! Код провода (`ErrorCode`) даёт `HasCode`: атомарная ошибка — одной
//! строкой `code!(T => Code)` рядом с определением, `Err` операции —
//! `code!(Err => [Variant, …])`, делегируя по варианту.

use std::fmt;

use eva::{data, time::HumanDuration};

use crate::wire::ErrorCode;

/// `code!(T => Code)` — атомарная ошибка с постоянным кодом;
/// `code!(Err => [A, B])` — `Err` операции делегирует варианту.
macro_rules! code {
    ($ty:ty => [$($variant:ident),* $(,)?]) => {
        impl $crate::errors::HasCode for $ty {
            fn code(&self) -> $crate::wire::ErrorCode {
                match *self {$(
                    Self::$variant(ref e) => $crate::errors::HasCode::code(e),
                )*}
            }

            fn retry_after(&self) -> ::core::option::Option<::eva::time::HumanDuration> {
                match *self {$(
                    Self::$variant(ref e) => $crate::errors::HasCode::retry_after(e),
                )*}
            }
        }
    };
    ($ty:ty => $code:ident) => {
        impl $crate::errors::HasCode for $ty {
            fn code(&self) -> $crate::wire::ErrorCode {
                $crate::wire::ErrorCode::$code
            }
        }
    };
}

pub mod ext;

pub mod blobs;
pub mod chats;
pub mod folders;
pub mod fs;
pub mod hands_grants;
pub mod inference;
pub mod memory;
pub mod modules;
pub mod persons;
pub mod sleep;
pub mod sqlite;
pub mod surfaces;
pub mod tools;
pub mod turns;

pub type Result<O, E> = ::core::result::Result<O, Generic<E>>;
pub type AuxResult<O> = ::core::result::Result<O, Aux>;

/// Код ошибки на проводе: классифицирует сторона, знающая причину.
pub trait HasCode {
    fn code(&self) -> ErrorCode;

    /// Когда повторить; знают только ошибки квоты и временной недоступности провайдера.
    fn retry_after(&self) -> Option<HumanDuration> {
        None
    }
}

/// Операция, у которой своих ошибок нет.
#[data(copy, ord, display("impossible"), error)]
#[derive(Hash)]
pub enum Impossible {}

impl HasCode for Impossible {
    fn code(&self) -> ErrorCode {
        match *self {}
    }
}

/// Беда подсистемы, общая для всех операций.
#[data(error)]
pub enum Aux {
    /// База недоступна или запрос к ней не прошёл.
    #[display("{_0}")]
    Db(String),
    /// Caller не вправе делать это (`Policy`).
    #[display("forbidden: {_0}")]
    Forbidden(String),
    /// Объект не существует, и своей ошибки у операции для этого нет.
    #[display("{_0} not found")]
    NotFound(String),
    /// Внешняя машина или модуль недостижимы — можно пойти другим путём.
    #[display("unavailable: {_0}")]
    Unavailable(String),
    /// Аргументы не прошли валидацию.
    #[display("invalid: {_0}")]
    Invalid(String),
    /// Лимит или квота.
    #[display("quota: {_0}")]
    Quota(String),
    /// Состояние изменилось под вызовом.
    #[display("conflict: {_0}")]
    Conflict(String),
    /// Баг ядра.
    #[display("{_0}")]
    Internal(String),
}

impl Aux {
    pub fn db(e: impl fmt::Display) -> Self {
        Self::Db(e.to_string())
    }

    pub fn forbidden(what: impl fmt::Display) -> Self {
        Self::Forbidden(what.to_string())
    }

    pub fn internal(e: impl fmt::Display) -> Self {
        Self::Internal(e.to_string())
    }
}

impl HasCode for Aux {
    fn code(&self) -> ErrorCode {
        match self {
            Self::Forbidden(_) => ErrorCode::Forbidden,
            Self::NotFound(_) => ErrorCode::NotFound,
            Self::Invalid(_) => ErrorCode::InvalidParams,
            Self::Conflict(_) => ErrorCode::Conflict,
            Self::Quota(_) => ErrorCode::Quota,
            Self::Unavailable(_) => ErrorCode::Unavailable,
            Self::Db(_) | Self::Internal(_) => ErrorCode::Internal,
        }
    }
}

/// Ошибка операции: либо своя (`Spec`), либо общая (`Aux`).
///
/// Без `Timestamp` в атомарных ошибках: untagged-разбор идёт через буфер
/// serde, где он из msgpack не читается.
#[data]
#[serde(untagged)]
pub enum Generic<S> {
    Aux(#[from] Aux),
    Spec(S),
}

impl<S> From<Impossible> for Generic<S> {
    fn from(value: Impossible) -> Self {
        match value {}
    }
}

impl<S: HasCode> HasCode for Generic<S> {
    fn code(&self) -> ErrorCode {
        match self {
            Self::Aux(aux) => aux.code(),
            Self::Spec(spec) => spec.code(),
        }
    }

    fn retry_after(&self) -> Option<HumanDuration> {
        match self {
            Self::Aux(aux) => aux.retry_after(),
            Self::Spec(spec) => spec.retry_after(),
        }
    }
}

impl<S: fmt::Display> fmt::Display for Generic<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Aux(aux) => aux.fmt(f),
            Self::Spec(spec) => spec.fmt(f),
        }
    }
}

impl<S: fmt::Debug + fmt::Display> std::error::Error for Generic<S> {}
