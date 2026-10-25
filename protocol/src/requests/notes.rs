//! Блокнот чата: только чтение, режим не пересчитывается.

use crate::requests::Response;
use eva::service;

use crate::{
    errors,
    types::{
        ChatId,
        note::{Mode, Note},
    },
};

#[service(export, result = Response)]
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
}
