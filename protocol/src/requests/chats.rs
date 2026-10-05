//! Чаты. Полный список операций — `docs/design/11-api-library.md §3`.

use eva::{data, time::Timestamp};
use serde_json::Value;

use crate::{
    errors,
    requests::{Limit, Patch},
    types::{
        ChatId, Effort, FolderId, MessageId, ModelId, SurfaceId, Thinking,
        chat::{self, Chat, ExternalKind, Message, SearchHit, Segment},
        content::Block,
        folder,
        turn::{ActorRef, TurnState, TurnView},
    },
};

pub use crate::types::chat::Selector;

/// Сколько хитов поиска вернуть.
#[eva::int(u8, 1..=50)]
pub enum SearchLimit {}

impl Default for SearchLimit {
    fn default() -> Self {
        Self::POS10
    }
}

/// Сколько реплик истории прочитать за раз.
#[eva::int(u16, 1..=200)]
pub enum HistoryCount {}

impl Default for HistoryCount {
    fn default() -> Self {
        Self::POS50
    }
}

pub mod ingest {
    use super::*;

    /// Чат у поверхности.
    #[data]
    pub struct ExternalChat {
        pub id: String,
        pub kind: ExternalKind,
        pub title: Option<String>,
    }

    /// Модель на сообщение; учитывается, только если актор — хозяин.
    #[data]
    #[derive(Default)]
    pub struct Override {
        pub model: Option<ModelId>,
        pub effort: Option<Effort>,
        pub thinking: Option<Thinking>,
    }

    #[data]
    pub struct Args {
        pub surface: SurfaceId,
        pub external_chat: ExternalChat,
        /// `None` — неизвестный актор, никогда не хозяин.
        pub sender: Option<ActorRef>,
        pub content: Vec<Block>,
        /// Повтор с тем же id идемпотентен.
        pub external_id: Option<String>,
        pub reply_to_external_id: Option<String>,
        /// Якорь поверхности: ядру непрозрачен, возвращается тулам как есть.
        pub anchor: Option<Value>,
        pub r#override: Option<Override>,
        #[serde(default)]
        pub silent: bool,
    }

    #[data]
    pub struct Ok {
        pub chat: ChatId,
        pub message_id: MessageId,
        pub turn: TurnState,
    }

    #[data(error, display("_"))]
    pub enum Err {}

    code!(Err => []);
}

pub mod create {
    use super::*;

    #[data]
    pub struct Args {
        pub surface: SurfaceId,
        /// `None` — `Unsorted`.
        pub folder: Option<FolderId>,
        pub external_id: Option<String>,
        /// Нужен при `external_id`; `None` — `direct`.
        pub external_kind: Option<ExternalKind>,
        /// `None` — `chats.default_kind`.
        pub kind: Option<chat::Kind>,
        pub title: Option<String>,
        pub project: Option<String>,
        pub workdir: Option<String>,
        pub workdir_on: Option<String>,
    }

    #[data]
    pub struct Ok {
        pub chat: Chat,
    }

    #[data(error, display("_"))]
    pub enum Err {}

    code!(Err => []);
}

pub mod get {
    use super::*;

    #[data]
    pub struct Args {
        pub chat: Selector,
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

    code!(Err => [NotFound]);
}

pub mod list {
    use super::*;

    #[data]
    #[derive(Default)]
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

    code!(Err => []);
}

pub mod update {
    use super::*;

    #[serde_with::apply(Patch => #[serde(default)])]
    #[data]
    #[derive(Default)]
    pub struct Update {
        pub title: Patch<Option<String>>,
        /// `true` делает чат persistent.
        pub pinned: Patch<bool>,
        pub alerted: Patch<bool>,
        pub kind: Patch<chat::Kind>,
        pub trusted: Patch<Option<bool>>,
        pub model: Patch<Option<ModelId>>,
        pub effort: Patch<Option<Effort>>,
        pub thinking: Patch<Option<Thinking>>,
        pub compact_at: Patch<Option<f32>>,
        pub project: Patch<Option<String>>,
        pub workdir: Patch<Option<String>>,
        pub workdir_on: Patch<Option<String>>,
    }

    #[data]
    pub struct Args {
        pub chat: ChatId,
        #[serde(default)]
        pub update: Update,
    }

    #[data]
    pub struct Ok {
        pub chat: Chat,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::chats::NotFound),
        #[display("{_0}")]
        KindMonotonic(#[from] errors::chats::KindMonotonic),
        #[display("{_0}")]
        AgentTrust(#[from] errors::chats::AgentTrust),
    }

    code!(Err => [NotFound, KindMonotonic, AgentTrust]);
}

pub mod r#move {
    use super::*;

    #[data]
    pub struct Args {
        pub chat: ChatId,
        /// Только внутри surface чата.
        pub folder: folder::Selector,
        /// Подтверждает смену trust.
        #[serde(default)]
        pub confirm: bool,
    }

    #[data]
    pub struct Ok {
        pub chat: Chat,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::chats::NotFound),
        #[display("{_0}")]
        TrustChange(#[from] errors::chats::TrustChange),
        #[display("{_0}")]
        CrossSurface(#[from] errors::chats::CrossSurface),
    }

    code!(Err => [NotFound, TrustChange, CrossSurface]);
}

pub mod delete {
    use super::*;

    #[data]
    pub struct Args {
        pub chat: ChatId,
        /// Сразу стереть с логом и чатами подагентов, минуя корзину.
        #[serde(default)]
        pub purge: bool,
    }

    #[data]
    pub struct Ok {}

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::chats::NotFound),
        #[display("{_0}")]
        Undeletable(#[from] errors::chats::Undeletable),
    }

    code!(Err => [NotFound, Undeletable]);
}

pub mod restore {
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
        #[display("{_0}")]
        NotDeleted(#[from] errors::chats::NotDeleted),
    }

    code!(Err => [NotFound, NotDeleted]);
}

pub mod search {
    use super::*;

    #[data]
    pub struct Args {
        pub query: String,
        #[serde(default)]
        pub limit: SearchLimit,
        pub since: Option<Timestamp>,
        /// Фильтры `surface`/`folder`/`include_deleted` действуют только при полном scope.
        pub surface: Option<SurfaceId>,
        pub folder: Option<FolderId>,
        #[serde(default)]
        pub include_deleted: bool,
        pub view: Option<TurnView>,
    }

    #[data]
    pub struct Ok {
        pub hits: Vec<SearchHit>,
    }

    #[data(error, display("_"))]
    pub enum Err {}

    code!(Err => []);
}

pub mod history_read {
    use super::*;

    #[data]
    pub struct Args {
        pub chat: ChatId,
        #[serde(default)]
        pub from_seq: u64,
        /// Последние `count` реплик с seq меньше границы; без неё — вперёд от `from_seq`.
        pub before: Option<u64>,
        #[serde(default)]
        pub count: HistoryCount,
        pub view: Option<TurnView>,
    }

    #[data]
    pub struct Ok {
        pub messages: Vec<Message>,
        /// `None` — история прочитана до конца.
        pub next_seq: Option<u64>,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::chats::NotFound),
    }

    code!(Err => [NotFound]);
}

pub mod compact {
    use super::*;

    #[data]
    pub struct Args {
        pub chat: ChatId,
    }

    #[data]
    pub struct Ok {
        /// Новый сегмент, `reason = manual`.
        pub segment: Segment,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::chats::NotFound),
    }

    code!(Err => [NotFound]);
}

pub mod notify {
    use super::*;

    #[data]
    pub struct Args {
        /// Одна строка.
        pub subject: String,
        pub body: String,
        /// Чат-источник получает `alerted`.
        pub source_chat: Option<ChatId>,
    }

    #[data]
    pub struct Ok {
        pub message_id: MessageId,
    }

    #[data(error, display("_"))]
    pub enum Err {}

    code!(Err => []);
}
