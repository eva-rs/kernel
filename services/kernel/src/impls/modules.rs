use eva_core::service::modules::Modules;
use eva_protocol::requests::modules::{list, token_issue, token_revoke};

use crate::{components::Components, handling::modules as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Modules for Session<C> { project! {
    fn list() = h::List;
    fn token_issue() = h::TokenIssue;
    fn token_revoke() = h::TokenRevoke;
}}
