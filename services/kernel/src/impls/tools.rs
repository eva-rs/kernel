use eva_core::service::tools::Tools;
use eva_protocol::requests::tools::catalog;

use crate::{components::Components, handling::tools as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Tools for Session<C> { project! {
    fn catalog() = h::Catalog;
}}
