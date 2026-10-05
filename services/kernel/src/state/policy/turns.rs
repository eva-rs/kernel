//! Туры: права решает `chats().read` чата тура; `respond` дополнительно проверяет актора после `Policy` (`07 §4.2`).

use super::Policy;
use eva_core::Caller;
use eva_protocol::{
    errors::{Aux, AuxResult},
    types::{
        PersonId,
        turn::{Actor, ActorKind, PendingKind},
    },
};

impl Policy<'_> {
    pub fn respond(
        self,
        kind: PendingKind,
        actor: &Actor,
        turn_actor: Option<&PersonId>,
    ) -> AuxResult<()> {
        if matches!(self.caller(), Caller::Client { .. }) || actor.kind == ActorKind::Master {
            return Ok(());
        }
        if kind == PendingKind::Ask
            && actor
                .person_id
                .as_ref()
                .is_some_and(|id| Some(id) == turn_actor)
        {
            return Ok(());
        }
        Err(Aux::forbidden(
            "only the turn actor or master may answer this request",
        ))
    }
}
