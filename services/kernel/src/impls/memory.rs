use eva_core::service::memory::Memory;
use eva_protocol::requests::memory::{get, history, list, restore};

use crate::{components::Components, handling::memory as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Memory for Session<C> { project! {
    fn list() = h::List;
    fn get() = h::Get;
    fn history() = h::History;
    fn restore() = h::Restore;
}}
