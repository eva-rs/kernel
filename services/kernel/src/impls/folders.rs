use eva_core::service::folders::Folders;
use eva_protocol::requests::folders::{create, delete, list, update};

use crate::{components::Components, handling::folders as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Folders for Session<C> { project! {
    fn create() = h::Create;
    fn update() = h::Update;
    fn delete() = h::Delete;
    fn list() = h::List;
}}
