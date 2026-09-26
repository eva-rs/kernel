//! Хендлеры `tools.*` (`11 §3`).

use eva_protocol::{
    errors::Aux,
    requests::{Response, tools::catalog},
};

use crate::{
    components::Components,
    state::{Policy, State},
};

#[eva::endpoint]
#[fastrace::trace]
pub async fn Catalog<C: Components>(
    state: &mut State<C>,
    args: catalog::Args,
) -> Response<catalog::Ok, catalog::Err> {
    Policy::new(&state.caller).owner()?;
    let _ = args;
    Err(Aux::Unavailable("M4_PENDING: tools.catalog arrives with M4: tool registry".into()).into())
}
