use eva_core::service::modules::Modules;
use eva_protocol::requests::modules::{add, list, remove, token_issue, token_revoke};

use crate::{components::Components, handling::modules as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Modules for Session<C> { project! {
    fn list() = h::List;
    fn add() = h::Add;
    fn remove() = h::Remove;
    fn token_issue() = h::TokenIssue;
    fn token_revoke() = h::TokenRevoke;
}}
