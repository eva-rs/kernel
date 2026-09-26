use eva_core::service::surfaces::Surfaces;
use eva_protocol::requests::surfaces::{create, get, list, update};

use crate::{components::Components, handling::surfaces as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Surfaces for Session<C> { project! {
    fn create() = h::Create;
    fn get() = h::Get;
    fn list() = h::List;
    fn update() = h::Update;
}}
