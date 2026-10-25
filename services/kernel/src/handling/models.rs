//! Хендлеры `models.*` (`01 §8.2`, `11 §3`).

use eva_protocol::requests::{Models::list, Response};

use crate::{
    components::Components,
    convert,
    state::{Policy, State},
};

#[eva::endpoint]
#[fastrace::trace]
pub async fn List<C: Components>(
    state: &mut State<C>,
    args: list::Args,
) -> Response<list::Ok, list::Err> {
    let State { shared, caller, .. } = state;
    Policy::new(caller).owner()?;
    let list::Args {} = args;

    Ok(list::Ok {
        models: convert::model::from_registry(&shared.models()),
    })
}
