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
}

impl<C: Components> State<C> {
    pub fn new(shared: Shared<C>, world: WorldOf<C>, caller: Caller) -> Self {
        Self {
            shared,
            world,
            caller,
            db_conn: DbConn::default(),
        }
    }
}
