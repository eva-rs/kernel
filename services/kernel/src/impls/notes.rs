use eva_core::service::notes::Notes;
use eva_protocol::requests::notes::list;

use crate::{components::Components, handling::notes as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Notes for Session<C> { project! {
    fn list() = h::List;
}}
