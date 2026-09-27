//! Сессия ядра: `Session<C>(State<C>)` реализует все namespace-трейты
//! (`impls/`), отдавая шаги поверх хендлеров.

use eva_core::CallStep;
use eva_protocol::requests::Response;

use crate::{
    components::Components,
    handling::{IsEndpoint, ProvideState},
    state::State,
};

pub struct Session<C: Components>(pub State<C>);

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
