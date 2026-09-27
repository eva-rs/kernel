//! Операции: по файлу на домен, по модулю на операцию.
//!
//! Модуль операции объявляет `Args`, `Ok`, `Err`. У потока к вызывающему
//! `Ok = Items<Item, Err>`, `Item` — тип элемента; у загрузки `Args` несёт
//! `body: ByteStream`, а по проводу идёт `Meta`. Один и тот же модуль читают
//! библиотека, RPC-сервер, RPC-клиент и тул модели, поэтому форма вызова у
//! них одна.

use std::pin::Pin;

use eva::{bytes::Bytes, data};
use futures_core::Stream;
use serde::{Deserialize, Deserializer};

use crate::errors::{self, Aux};

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

/// Результат операции: свой `Err` либо беда подсистемы.
pub type Response<O, E> = errors::Result<O, E>;

/// `Ok` потока к вызывающему: отказ до первого элемента — `Err` самого
/// вызова, `Err` внутри потока — последний элемент, drop — отмена.
pub type Items<T, E> = Pin<Box<dyn Stream<Item = Response<T, E>> + Send>>;

/// Тело загрузки: чанки от вызывающего; обрыв источника — `Aux`.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, Aux>> + Send>>;

/// Сколько записей вернуть. Валидация — в типе, не в хендлере.
#[eva::int(u16, 1..=200)]
pub enum Limit {}

impl Default for Limit {
    fn default() -> Self {
        Self::POS50
    }
}

/// Частичное обновление: опущенное поле и `null` — «не трогать»,
/// `{"change": null}` у `Patch<Option<T>>` — очистить.
///
/// `#[serde_with::apply(Patch => #[serde(default)])]` обязан стоять **выше**
/// `#[data]`, иначе опущенное поле падает с `missing field`.
#[data(not(Deserialize))]
#[derive(Default)]
pub enum Patch<T> {
    #[default]
    Keep,
    Change(T),
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Patch<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Repr<T> {
            Keep,
            Change(T),
        }

        Ok(match Option::<Repr<T>>::deserialize(deserializer)? {
            None | Some(Repr::Keep) => Self::Keep,
            Some(Repr::Change(value)) => Self::Change(value),
        })
    }
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

/// Страница журналов запусков крона и сна: строки тяжёлые, дефолт меньше.
#[eva::int(u16, 1..=200)]
pub enum RunsLimit {}

impl Default for RunsLimit {
    fn default() -> Self {
        Self::POS20
    }
}
