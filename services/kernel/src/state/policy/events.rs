//! Доставка событий и хуков модулям (`06 §7`): `service` — только по своим
//! surface, топик без surface — только клиентам.

use eva_core::Caller;
use eva_protocol::{
    errors::{Aux, AuxResult},
    wire::events::Topic,
};

use crate::{db::surfaces::SurfaceRef, state::policy::Policy};

#[derive(Debug, Clone, Copy)]
pub struct EventsPolicy<'a>(Policy<'a>);

impl<'a> Policy<'a> {
    pub const fn events(self) -> EventsPolicy<'a> {
        EventsPolicy(self)
    }
}

impl EventsPolicy<'_> {
    /// Получает ли сессия событие `topic` по `surface`; ядро событий не слушает.
    pub fn deliver(self, topic: &Topic, surface: Option<&SurfaceRef>) -> AuxResult<()> {
        match (self.0.caller(), surface) {
            (Caller::Client { .. }, _) => Ok(()),
            (Caller::Service { .. }, Some(surface)) => self.0.surfaces().own(surface),
            (Caller::Service { .. }, None) => Err(Aux::forbidden(format_args!(
                "{topic} is for client modules only"
            ))),
            (Caller::Kernel, _) => Err(Aux::forbidden("the kernel has no event subscription")),
        }
    }

    /// Хук тура чата на `surface` (`06 §7`): `service` — только свои surface,
    /// `client` — любые.
    pub fn hook(self, surface: &SurfaceRef) -> AuxResult<()> {
        match self.0.caller() {
            Caller::Client { .. } => Ok(()),
            Caller::Service { .. } => self.0.surfaces().own(surface),
            Caller::Kernel => Err(Aux::forbidden("the kernel hooks nothing")),
        }
    }
}
