//! Чаты. Полный список операций — `docs/design/11-api-library.md §3`.

use eva::data;

use crate::{
    errors,
    requests::Limit,
    types::{ChatId, FolderId, SurfaceId, chat::Chat},
};

pub mod get {
    use super::*;

    #[data]
    pub struct Args {
        pub chat: ChatId,
    }

    #[data]
    pub struct Ok {
        pub chat: Chat,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::chats::NotFound),
    }
}

pub mod list {
    use super::*;

    #[data]
    pub struct Args {
        pub surface: Option<SurfaceId>,
        pub folder: Option<FolderId>,
        /// Показать только удалённые.
        #[serde(default)]
        pub deleted: bool,
        /// Только чаты с открытым туром — реконнект клиента.
        #[serde(default)]
        pub live: bool,
        #[serde(default)]
        pub limit: Limit,
        pub after: Option<ChatId>,
    }

    #[data]
    pub struct Ok {
        pub found: Vec<Chat>,
    }

    #[data(error, display("_"))]
    pub enum Err {}
}
