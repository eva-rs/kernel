use eva_core::service::sqlite::Sqlite;
use eva_protocol::requests::sqlite::{exec, query, tx};

use crate::{components::Components, handling::sqlite as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Sqlite for Session<C> { project! {
    fn exec() = h::Exec;
    fn query() = h::Query;
    fn tx() = h::Tx;
}}
