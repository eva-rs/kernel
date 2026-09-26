use eva_core::service::agents::Agents;
use eva_protocol::requests::agents::list;

use crate::{components::Components, handling::agents as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Agents for Session<C> { project! {
    fn list() = h::List;
}}
