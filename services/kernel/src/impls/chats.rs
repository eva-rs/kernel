use eva_core::service::chats::Chats;
use eva_protocol::requests::chats::{
    compact, create, delete, get, history_read, ingest, list, r#move, notify, restore, search,
    update,
};

use crate::{components::Components, handling::chats as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Chats for Session<C> { project! {
    fn ingest() = h::Ingest;
    fn create() = h::Create;
    fn get() = h::Get;
    fn list() = h::List;
    fn update() = h::Update;
    fn r#move() = h::Move;
    fn delete() = h::Delete;
    fn restore() = h::Restore;
    fn search() = h::Search;
    fn history_read() = h::HistoryRead;
    fn compact() = h::Compact;
    fn notify() = h::Notify;
}}
