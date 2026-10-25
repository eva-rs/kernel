//! Реестр тулов глазами клиента хозяина.

use crate::requests::Response;
use eva::service;

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

#[service(export, result = Response)]
pub trait Tools {
    #[args(derive(Default))]
    #[errors(errors::tools::PathNotFound)]
    fn catalog(
        /// Поддерево; `None` — корень.
        path: Option<String>,
        #[serde(default)] depth: Depth,
        /// Посчитать `exposure` по контексту последнего шага чата.
        chat: Option<ChatId>,
    ) -> ok! {
        entries: Vec<CatalogEntry>,
    };
    embed! {
        code!(Err => [PathNotFound]);
    }
}
