//! Хендлеры `turns.*` (`11 §3`).

use eva_core::Items;
use eva_protocol::{
    errors::Aux,
    requests::{
        Response,
        turns::{append_silent, attach, cancel, delivered, respond, start},
    },
};

use crate::{components::Components, state::State};

#[eva::endpoint]
#[fastrace::trace]
pub async fn Start<C: Components>(
    state: &mut State<C>,
    args: start::Args,
) -> Response<start::Ok, start::Err> {
    let _ = (state, args);
    Err(Aux::Unavailable("M2_PENDING: turns.start arrives with M2: turn engine".into()).into())
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Attach<C: Components>(
    state: &mut State<C>,
    args: attach::Args,
) -> Response<Items<attach::Item, attach::Err>, attach::Err> {
    let _ = (state, args);
    Err(Aux::Unavailable("M2_PENDING: turns.attach arrives with M2: turn engine".into()).into())
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn AppendSilent<C: Components>(
    state: &mut State<C>,
    args: append_silent::Args,
) -> Response<append_silent::Ok, append_silent::Err> {
    let _ = (state, args);
    Err(
        Aux::Unavailable("M2_PENDING: turns.append_silent arrives with M2: turn engine".into())
            .into(),
    )
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Delivered<C: Components>(
    state: &mut State<C>,
    args: delivered::Args,
) -> Response<delivered::Ok, delivered::Err> {
    let _ = (state, args);
    Err(Aux::Unavailable("M2_PENDING: turns.delivered arrives with M2: turn engine".into()).into())
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Cancel<C: Components>(
    state: &mut State<C>,
    args: cancel::Args,
) -> Response<cancel::Ok, cancel::Err> {
    let _ = (state, args);
    Err(Aux::Unavailable("M2_PENDING: turns.cancel arrives with M2: turn engine".into()).into())
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Respond<C: Components>(
    state: &mut State<C>,
    args: respond::Args,
) -> Response<respond::Ok, respond::Err> {
    let _ = (state, args);
    Err(Aux::Unavailable("M2_PENDING: turns.respond arrives with M2: turn engine".into()).into())
}
