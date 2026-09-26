use eva_core::service::kv::Kv;
use eva_protocol::requests::kv::{del, get, list, set};

use crate::{components::Components, handling::kv as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Kv for Session<C> { project! {
    fn get() = h::Get;
    fn set() = h::Set;
    fn del() = h::Del;
    fn list() = h::List;
}}
