//! Сессия ядра: `Session<C>(State<C>)` реализует все namespace-трейты
//! (`impls/`), отдавая шаги поверх хендлеров.

use eva_core::{CallStep, Items, StreamStep, UploadStep};

use crate::{
    components::Components,
    handling::{IsEndpoint, ProvideState, ProvideStream, ProvideUpload, Upload},
    state::State,
};

pub struct Session<C: Components>(pub State<C>);

impl<C: Components> Session<C> {
    pub const fn new(state: State<C>) -> Self {
        Self(state)
    }

    pub fn with_state<I, E, Ok, Err>(&mut self, handler: E) -> impl CallStep<I, Ok = Ok, Err = Err>
    where
        I: Send,
        E: IsEndpoint<I, C, Ok = Ok, Err = Err>,
    {
        ProvideState::new(&mut self.0, handler)
    }

    pub fn with_stream<I, E, Item, Err>(
        &mut self,
        handler: E,
    ) -> impl StreamStep<I, Item = Item, Err = Err>
    where
        I: Send,
        E: IsEndpoint<I, C, Ok = Items<Item, Err>, Err = Err>,
    {
        ProvideStream::new(&mut self.0, handler)
    }

    pub fn with_upload<I, E, Ok, Err>(
        &mut self,
        handler: E,
    ) -> impl UploadStep<I, Ok = Ok, Err = Err>
    where
        I: Send,
        E: IsEndpoint<Upload<I>, C, Ok = Ok, Err = Err>,
    {
        ProvideUpload::new(&mut self.0, handler)
    }
}
