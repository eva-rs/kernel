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

mod agents;
mod blobs;
mod chats;
mod cron;
mod folders;
mod fs;
mod hands_grants;
mod inference;
mod kv;
mod memory;
mod models;
mod modules;
mod notes;
mod persons;
mod sleep;
mod sqlite;
mod stats;
mod surfaces;
mod tools;
mod turns;

pub use self::{
    agents::Agents,
    blobs::Blobs,
    chats::{Chats, HistoryCount, SearchLimit, Selector},
    cron::Cron,
    folders::Folders,
    fs::{Entry, EntryKind, Fs},
    hands_grants::HandsGrants,
    inference::Inference,
    kv::Kv,
    memory::Memory,
    models::Models,
    modules::Modules,
    notes::Notes,
    persons::Persons,
    sleep::Sleep,
    sqlite::{Cell, Sqlite, Statement},
    stats::{Stats, Top},
    surfaces::Surfaces,
    tools::{Depth, Tools},
    turns::Turns,
};

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
