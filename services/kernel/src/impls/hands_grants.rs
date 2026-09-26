use eva_core::service::hands_grants::HandsGrants;
use eva_protocol::requests::hands_grants::{list, revoke};

use crate::{components::Components, handling::hands_grants as h, session::Session};

#[rustfmt::skip]
impl<C: Components> HandsGrants for Session<C> { project! {
    fn list() = h::List;
    fn revoke() = h::Revoke;
}}
