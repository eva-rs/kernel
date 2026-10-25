//! Чаты. Полный список операций — `docs/design/11-api-library.md §3`.

use crate::requests::Response;
use eva::{data, service, time::Timestamp};

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

#[service(export, result = Response)]
pub trait Chats {
    #[flags(mutating)]
    fn ingest(
        surface: SurfaceId,
        external_chat: this![ExternalChat],
        /// `None` — неизвестный актор, никогда не хозяин.
        sender: Option<ActorRef>,
        content: Vec<Block>,
        /// Повтор с тем же id идемпотентен.
        external_id: Option<String>,
        reply_to_external_id: Option<String>,
        /// Якорь поверхности: ядру непрозрачен, возвращается тулам как есть.
        anchor: Option<Value>,
        r#override: Option<this![Override]>,
        #[serde(default)] silent: bool,
    ) -> ok! {
        chat: ChatId,
        message_id: MessageId,
        turn: TurnState,
    };
    embed! {
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

        code!(Err => []);
    }

    #[flags(mutating)]
    fn create(
        surface: SurfaceId,
        /// `None` — `Unsorted`.
        folder: Option<FolderId>,
        external_id: Option<String>,
        /// Нужен при `external_id`; `None` — `direct`.
        external_kind: Option<ExternalKind>,
        /// `None` — `chats.default_kind`.
        kind: Option<chat::Kind>,
        title: Option<String>,
        project: Option<String>,
        workdir: Option<String>,
        workdir_on: Option<String>,
    ) -> ok! {
        chat: Chat,
    };
    embed! {
        code!(Err => []);
    }

    #[errors(errors::chats::NotFound)]
    fn get(
        chat: Selector,
    ) -> ok! {
        chat: Chat,
    };
    embed! {
        code!(Err => [NotFound]);
    }

    #[args(derive(Default))]
    fn list(
        surface: Option<SurfaceId>,
        folder: Option<FolderId>,
        /// Показать только удалённые.
        #[serde(default)]
        deleted: bool,
        /// Только чаты с открытым туром — реконнект клиента.
        #[serde(default)]
        live: bool,
        #[serde(default)] limit: Limit,
        after: Option<ChatId>,
    ) -> ok! {
        found: Vec<Chat>,
    };
    embed! {
        code!(Err => []);
    }

    #[flags(mutating)]
    #[errors(
        errors::chats::NotFound,
        errors::chats::KindMonotonic,
        errors::chats::AgentTrust
    )]
    fn update(
        chat: ChatId,
        #[serde(default)] update: this![Update],
    ) -> ok! {
        chat: Chat,
    };
    embed! {
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

        code!(Err => [NotFound, KindMonotonic, AgentTrust]);
    }

    #[flags(mutating)]
    #[errors(
        errors::chats::NotFound,
        errors::chats::TrustChange,
        errors::chats::CrossSurface
    )]
    fn r#move(
        chat: ChatId,
        /// Только внутри surface чата.
        folder: folder::Selector,
        /// Подтверждает смену trust.
        #[serde(default)]
        confirm: bool,
    ) -> ok! {
        chat: Chat,
    };
    embed! {
        code!(Err => [NotFound, TrustChange, CrossSurface]);
    }

    #[flags(mutating)]
    #[errors(errors::chats::NotFound, errors::chats::Undeletable)]
    fn delete(
        chat: ChatId,
        /// Сразу стереть с логом и чатами подагентов, минуя корзину.
        #[serde(default)]
        purge: bool,
    ) -> ok! {};
    embed! {
        code!(Err => [NotFound, Undeletable]);
    }

    #[flags(mutating)]
    #[errors(errors::chats::NotFound, errors::chats::NotDeleted)]
    fn restore(
        chat: ChatId,
    ) -> ok! {
        chat: Chat,
    };
    embed! {
        code!(Err => [NotFound, NotDeleted]);
    }

    fn search(
        query: String,
        #[serde(default)] limit: SearchLimit,
        since: Option<Timestamp>,
        /// Фильтры `surface`/`folder`/`include_deleted` действуют только при полном scope.
        surface: Option<SurfaceId>,
        folder: Option<FolderId>,
        #[serde(default)] include_deleted: bool,
        view: Option<TurnView>,
    ) -> ok! {
        hits: Vec<SearchHit>,
    };
    embed! {
        code!(Err => []);
    }

    #[errors(errors::chats::NotFound)]
    fn history_read(
        chat: ChatId,
        #[serde(default)] from_seq: u64,
        /// Последние `count` реплик с seq меньше границы; без неё — вперёд от `from_seq`.
        before: Option<u64>,
        #[serde(default)] count: HistoryCount,
        view: Option<TurnView>,
    ) -> ok! {
        messages: Vec<Message>,
        /// `None` — история прочитана до конца.
        next_seq: Option<u64>,
    };
    embed! {
        code!(Err => [NotFound]);
    }

    #[flags(mutating)]
    #[errors(errors::chats::NotFound)]
    fn compact(
        chat: ChatId,
    ) -> ok! {
        /// Новый сегмент, `reason = manual`.
        segment: Segment,
    };
    embed! {
        code!(Err => [NotFound]);
    }

    #[flags(mutating)]
    fn notify(
        /// Одна строка.
        subject: String,
        body: String,
        /// Чат-источник получает `alerted`.
        source_chat: Option<ChatId>,
    ) -> ok! {
        message_id: MessageId,
    };
    embed! {
        code!(Err => []);
    }
}
