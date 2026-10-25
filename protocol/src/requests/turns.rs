//! Туры: старт, лента, доставка, отмена, ответы на pending.

use crate::requests::Response;
use eva::service;

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

#[service(result = Response)]
pub trait Turns {
    #[flags(mutating)]
    #[errors(errors::chats::NotFound)]
    fn start(
        chat: ChatId,
        /// `None` — неизвестный актор.
        actor: Option<ActorRef>,
        content: Vec<Block>,
        external_id: Option<String>,
        reply_to_external_id: Option<String>,
        anchor: Option<Value>,
        model: Option<ModelId>,
        effort: Option<Effort>,
        thinking: Option<Thinking>,
        /// Тулы на один тур, исполняются модулем-вызывателем; в реестр не попадают.
        #[serde(default)]
        extra_tools: Vec<ToolDecl>,
    ) -> ok! {
        message_id: MessageId,
        turn: TurnState,
    };
    embed! {
        code!(Err => [NotFound]);
    }

    #[flags(stream)]
    #[errors(errors::chats::NotFound)]
    fn attach(
        chat: ChatId,
        /// История — события с `events.id` больше этого; `None` — без истории,
        /// только текущее состояние и живое.
        since_seq: Option<EventId>,
    ) -> ok!(
        #[data(not(Debug, serde, schemars, Clone, PartialEq))]
        Items<this![Item], this![Err]>
    );
    embed! {
        pub type Item = TurnItem;

        code!(Err => [NotFound]);
    }

    #[flags(mutating)]
    #[errors(errors::chats::NotFound)]
    fn append_silent(
        chat: ChatId,
        actor: Option<ActorRef>,
        content: Vec<Block>,
        external_id: Option<String>,
    ) -> ok! {
        message_id: MessageId,
    };
    embed! {
        code!(Err => [NotFound]);
    }

    #[flags(mutating)]
    #[errors(
        errors::chats::NotFound,
        errors::chats::MessageNotFound,
        errors::chats::AlreadyDelivered
    )]
    fn delivered(
        chat: ChatId,
        message_id: MessageId,
        /// Id реплики Евы у поверхности: цель будущих `↩your#<N>`.
        external_id: String,
    ) -> ok! {};
    embed! {
        code!(Err => [NotFound, MessageNotFound, AlreadyDelivered]);
    }

    #[flags(mutating)]
    #[errors(errors::chats::NotFound)]
    fn cancel(chat: ChatId) -> ok! {};
    embed! {
        code!(Err => [NotFound]);
    }

    #[flags(mutating)]
    #[errors(errors::turns::PendingNotFound, errors::turns::AlreadyResolved)]
    fn respond(
        pending: PendingId,
        answer: Answer,
        /// Резолвится по surface чата pending, не по полям ссылки.
        actor: Option<ActorRef>,
    ) -> ok! {};
    embed! {
        code!(Err => [PendingNotFound, AlreadyResolved]);
    }
}
