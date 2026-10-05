//! Хендлеры: по `#[eva::endpoint]` на операцию, файл на домен (`11 §5`).
//! `ProvideState` ниже превращает хендлер над `&mut State<C>` в шаг сессии.

use eva::trait_set;
use eva_core::{CallStep, service};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::Response,
};

use crate::{components::Components, state::State};

pub mod agents;
pub mod blobs;
pub mod chats;
pub mod cron;
pub mod folders;
pub mod fs;
pub mod hands_grants;
pub mod inference;
pub mod kv;
pub mod memory;
pub mod models;
pub mod modules;
pub mod notes;
pub mod persons;
pub mod sleep;
pub mod sqlite;
pub mod stats;
pub mod surfaces;
pub mod text;
pub mod tools;
pub mod turns;

trait_set! {
    pub trait IsEndpoint<I, C: Components> = for<'a> service::IsEndpoint<I, &'a mut State<C>>;
}

pub struct ProvideState<'a, C: Components, H> {
    state: &'a mut State<C>,
    handler: H,
}

impl<'a, C: Components, H> ProvideState<'a, C, H> {
    pub const fn new(state: &'a mut State<C>, handler: H) -> Self {
        Self { state, handler }
    }
}

impl<C, H, I, Ok, Err> CallStep<I> for ProvideState<'_, C, H>
where
    I: Send,
    C: Components,
    H: IsEndpoint<I, C, Ok = Ok, Err = Err>,
{
    type Output = Response<Ok, Err>;

    async fn call(&mut self, args: I) -> Response<Ok, Err> {
        let reason = match self.handler.call(self.state, args).await {
            Err(Generic::Aux(Aux::Forbidden(reason))) => reason,
            res => return res,
        };
        refused(self.state).await;
        Err(Generic::Aux(Aux::Forbidden(reason)))
    }
}

/// Отказ `Policy` модулю — в `modules.forbidden` (`09 §5`); ответ отпускается до `await`, иначе будущему нужен `Send` от `Ok`/`Err`.
async fn refused<C: Components>(state: &State<C>) {
    crate::subsystems::modules::forbidden(&state.shared, &state.caller).await;
}
