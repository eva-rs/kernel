//! Сессия ядра: `Session<C>(State<C>)` реализует все namespace-трейты
//! (`impls/`), отдавая шаги поверх хендлеров.

use eva::handling::CallStep;
use eva_protocol::requests::Response;

use crate::{
    components::Components,
    handling::{IsEndpoint, ProvideState},
    shared::Shared,
    state::State,
};

pub struct Session<C: Components>(pub State<C>);

pub(crate) fn internal<C: Components>(shared: &Shared<C>) -> eva_core::Session<Session<C>> {
    internal_with_chat(shared, None)
}

pub(crate) fn for_tool<C: Components>(
    shared: &Shared<C>,
    chat: eva_protocol::types::ChatId,
) -> eva_core::Session<Session<C>> {
    internal_with_chat(shared, Some(chat))
}

fn internal_with_chat<C: Components>(
    shared: &Shared<C>,
    chat: Option<eva_protocol::types::ChatId>,
) -> eva_core::Session<Session<C>> {
    let caller = crate::state::policy::kernel_caller();
    let mut state = State::new(shared.clone(), shared.make_world(), caller.clone());
    state.tool_chat = chat;
    state.tool_upload = chat.is_some();
    eva_core::Session::new(Session::new(state), caller)
}

impl<C: Components> Session<C> {
    pub const fn new(state: State<C>) -> Self {
        Self(state)
    }

    pub fn with_state<I, E, Ok, Err>(
        &mut self,
        handler: E,
    ) -> impl CallStep<I, Output = Response<Ok, Err>>
    where
        I: Send,
        E: IsEndpoint<I, C, Ok = Ok, Err = Err>,
    {
        ProvideState::new(&mut self.0, handler)
    }
}
