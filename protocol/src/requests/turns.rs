//! Туры: старт, лента, доставка, отмена, ответы на pending.

use eva::data;
use serde_json::Value;

use crate::{
    errors,
    requests::Items,
    types::{
        ChatId, Effort, EventId, MessageId, ModelId, PendingId, Thinking,
        content::Block,
        tool::ToolDecl,
        turn::{ActorRef, Answer, TurnItem, TurnState},
    },
};

pub mod start {
    use super::*;

    #[data]
    pub struct Args {
        pub chat: ChatId,
        /// `None` — неизвестный актор.
        pub actor: Option<ActorRef>,
        pub content: Vec<Block>,
        pub external_id: Option<String>,
        pub reply_to_external_id: Option<String>,
        pub anchor: Option<Value>,
        pub model: Option<ModelId>,
        pub effort: Option<Effort>,
        pub thinking: Option<Thinking>,
        /// Тулы на один тур, исполняются модулем-вызывателем; в реестр не попадают.
        #[serde(default)]
        pub extra_tools: Vec<ToolDecl>,
    }

    #[data]
    pub struct Ok {
        pub message_id: MessageId,
        pub turn: TurnState,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::chats::NotFound),
    }

    code!(Err => [NotFound]);
}

pub mod attach {
    use super::*;

    #[data]
    pub struct Args {
        pub chat: ChatId,
        /// История — события с `events.id` больше этого; `None` — без истории,
        /// только текущее состояние и живое.
        pub since_seq: Option<EventId>,
    }

    pub type Item = TurnItem;

    pub type Ok = Items<Item, Err>;

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::chats::NotFound),
    }

    code!(Err => [NotFound]);
}

pub mod append_silent {
    use super::*;

    #[data]
    pub struct Args {
        pub chat: ChatId,
        pub actor: Option<ActorRef>,
        pub content: Vec<Block>,
        pub external_id: Option<String>,
    }

    #[data]
    pub struct Ok {
        pub message_id: MessageId,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::chats::NotFound),
    }

    code!(Err => [NotFound]);
}

pub mod delivered {
    use super::*;

    #[data]
    pub struct Args {
        pub chat: ChatId,
        pub message_id: MessageId,
        /// Id реплики Евы у поверхности: цель будущих `↩your#<N>`.
        pub external_id: String,
    }

    #[data]
    pub struct Ok {}

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::chats::NotFound),
        #[display("{_0}")]
        MessageNotFound(#[from] errors::chats::MessageNotFound),
        #[display("{_0}")]
        AlreadyDelivered(#[from] errors::chats::AlreadyDelivered),
    }

    code!(Err => [NotFound, MessageNotFound, AlreadyDelivered]);
}

pub mod cancel {
    use super::*;

    #[data]
    pub struct Args {
        pub chat: ChatId,
    }

    #[data]
    pub struct Ok {}

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::chats::NotFound),
    }

    code!(Err => [NotFound]);
}

pub mod respond {
    use super::*;

    #[data]
    pub struct Args {
        pub pending: PendingId,
        pub answer: Answer,
        /// Резолвится по surface чата pending, не по полям ссылки.
        pub actor: Option<ActorRef>,
    }

    #[data]
    pub struct Ok {}

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        PendingNotFound(#[from] errors::turns::PendingNotFound),
        #[display("{_0}")]
        AlreadyResolved(#[from] errors::turns::AlreadyResolved),
    }

    code!(Err => [PendingNotFound, AlreadyResolved]);
}
