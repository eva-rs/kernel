//! Состояние вызова (`11 §5`): общее ядро, мир, caller и ленивое
//! читающее соединение. `Policy` строится в хендлере, здесь не хранится.

use eva_core::Caller;

use crate::{
    components::Components,
    shared::{Shared, WorldOf},
};

pub mod db_conn;
pub mod policy;

pub use self::{db_conn::DbConn, policy::Policy};

#[non_exhaustive]
pub struct State<C: Components> {
    pub shared: Shared<C>,
    pub world: WorldOf<C>,
    pub caller: Caller,
    pub db_conn: DbConn,
    /// Удаление чата из его тула не может ждать закрытия того же тура.
    pub(crate) tool_chat: Option<eva_protocol::types::ChatId>,
    /// Происхождение blob'ов инструмента задаётся внутри общего API.
    pub(crate) tool_upload: bool,
}

impl<C: Components> State<C> {
    pub fn new(shared: Shared<C>, world: WorldOf<C>, caller: Caller) -> Self {
        Self {
            shared,
            world,
            caller,
            db_conn: DbConn::default(),
            tool_chat: None,
            tool_upload: false,
        }
    }
}
