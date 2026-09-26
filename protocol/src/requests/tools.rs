//! Реестр тулов глазами клиента хозяина.

use eva::data;

use crate::{
    errors,
    types::{ChatId, tool::CatalogEntry},
};

/// Глубина поддерева каталога.
#[eva::int(u8, 1..=16)]
pub enum Depth {}

impl Default for Depth {
    fn default() -> Self {
        Self::POS1
    }
}

pub mod catalog {
    use super::*;

    #[data]
    #[derive(Default)]
    pub struct Args {
        /// Поддерево; `None` — корень.
        pub path: Option<String>,
        #[serde(default)]
        pub depth: Depth,
        /// Посчитать `exposure` по контексту последнего шага чата.
        pub chat: Option<ChatId>,
    }

    #[data]
    pub struct Ok {
        pub entries: Vec<CatalogEntry>,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        PathNotFound(#[from] errors::tools::PathNotFound),
    }
}
