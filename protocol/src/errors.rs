//! Модель ошибок: у каждой операции свой `Err`, а общие беды системы —
//! `Aux`. На проводе и в библиотеке это один тип `Generic<E>`, поэтому
//! вызывающий различает «операция честно отказала» и «подсистема упала».

use std::fmt;

use eva::data;

pub mod ext;

pub mod chats;
pub mod surfaces;

pub type Result<O, E> = ::core::result::Result<O, Generic<E>>;
pub type AuxResult<O> = ::core::result::Result<O, Aux>;

/// Операция, у которой своих ошибок нет.
#[data(copy, ord, display("impossible"), error)]
#[derive(Hash)]
pub enum Impossible {}

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

/// Ошибка операции: либо своя (`Spec`), либо общая (`Aux`).
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

impl<S: fmt::Display> fmt::Display for Generic<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Aux(aux) => aux.fmt(f),
            Self::Spec(spec) => spec.fmt(f),
        }
    }
}

impl<S: fmt::Debug + fmt::Display> std::error::Error for Generic<S> {}
