use eva_core::service::models::Models;
use eva_protocol::requests::models::list;

use crate::{components::Components, handling::models as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Models for Session<C> { project! {
    fn list() = h::List;
}}
