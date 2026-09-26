use eva_core::service::persons::Persons;
use eva_protocol::requests::persons::{list, merge, rename, resolve, whitelist};

use crate::{components::Components, handling::persons as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Persons for Session<C> { project! {
    fn resolve() = h::Resolve;
    fn list() = h::List;
    fn rename() = h::Rename;
    fn merge() = h::Merge;
    fn whitelist() = h::Whitelist;
}}
