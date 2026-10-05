//! Блокнот чата: только чтение, режим не пересчитывается.

use eva::data;

use crate::{
    errors,
    types::{
        ChatId,
        note::{Mode, Note},
    },
};

pub mod list {
    use super::*;

    #[data]
    pub struct Args {
        pub chat: ChatId,
    }

    #[data]
    pub struct Ok {
        pub notes: Vec<Note>,
        pub mode: Mode,
        /// Master-туров в `squeeze`; вне `squeeze` = 0.
        pub squeeze_turns: u32,
        /// Токены inline-рендера секции: `fill` меряется по нему в любом режиме.
        pub tokens: u32,
        pub budget: u32,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::chats::NotFound),
    }

    code!(Err => [NotFound]);
}
