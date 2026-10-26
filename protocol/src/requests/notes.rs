//! Чтение, ручная правка и восстановление блокнота хозяином.

use crate::requests::Response;
use eva::service;

use crate::{
    errors,
    requests::Limit,
    types::{
        ChatId,
        note::{Fill, Mode, Note, Snapshot, tools},
    },
};

#[service(result = Response)]
pub trait Notes {
    #[errors(errors::chats::NotFound)]
    fn list(
        chat: ChatId,
    ) -> ok! {
        notes: Vec<Note>,
        mode: Mode,
        /// Master-туров в `squeeze`; вне `squeeze` = 0.
        squeeze_turns: u32,
        /// Токены inline-рендера секции: `fill` меряется по нему в любом режиме.
        tokens: u32,
        budget: u32,
    };
    embed! {
        code!(Err => [NotFound]);
    }

    #[flags(mutating)]
    #[error(crate::requests::Notes::list::Err)]
    fn put(
        chat: ChatId,
        name: String,
        description: String,
        body: String,
        /// `None` сохраняет булавку существующей заметки.
        pinned: Option<bool>,
    ) -> tools::save::Ok;

    #[flags(mutating)]
    #[error(crate::requests::Notes::list::Err)]
    fn delete(chat: ChatId, name: String) -> tools::delete::Ok;

    #[error(crate::requests::Notes::list::Err)]
    fn history(
        chat: ChatId,
        /// Только снимки с меньшим id; порядок от новых к старым.
        before: Option<i64>,
        #[serde(default)] limit: Limit,
    ) -> ok! {
        snapshots: Vec<Snapshot>,
    };

    #[error(crate::requests::Notes::list::Err)]
    fn snapshot(
        chat: ChatId,
        id: i64,
    ) -> ok! {
        snapshot: Snapshot,
        notes: Vec<Note>,
    };

    #[flags(mutating)]
    #[error(crate::requests::Notes::list::Err)]
    fn restore(
        chat: ChatId,
        id: i64,
    ) -> ok! {
        restored: i64,
        /// Состояние перед восстановлением; его тоже можно вернуть.
        backup: i64,
        fill: Fill,
    };
}
