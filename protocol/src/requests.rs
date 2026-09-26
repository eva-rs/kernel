//! Операции: по файлу на домен, по модулю на операцию.
//!
//! Модуль операции объявляет ровно три типа — `Args`, `Ok`, `Err`. Один и
//! тот же модуль читают библиотека, RPC-сервер, RPC-клиент и тул модели,
//! поэтому форма вызова у них одна.

use eva::data;

use crate::errors;

pub mod chats;
pub mod surfaces;

/// Результат операции: свой `Err` либо беда подсистемы.
pub type Response<O, E> = errors::Result<O, E>;

/// Сколько записей вернуть. Валидация — в типе, не в хендлере.
#[eva::int(u16, 1..=200)]
pub enum Limit {}

impl Default for Limit {
    fn default() -> Self {
        Self::POS50
    }
}

/// Частичное обновление: опущенное поле и `null` — «не трогать».
///
/// `#[serde_with::apply(Patch => #[serde(default)])]` обязан стоять **выше**
/// `#[data]`, иначе опущенное поле падает с `missing field`.
#[data]
#[derive(Default)]
pub enum Patch<T> {
    #[default]
    Keep,
    Change(T),
}

impl<T> Patch<T> {
    pub fn into_option(self) -> Option<T> {
        match self {
            Self::Keep => None,
            Self::Change(value) => Some(value),
        }
    }

    pub const fn is_keep(&self) -> bool {
        matches!(self, Self::Keep)
    }
}
