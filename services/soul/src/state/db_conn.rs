//! Читающее соединение сессии: берётся из пула на первом чтении.

use eva_protocol::errors::{Aux, AuxResult};

use crate::{
    components::Components,
    db::{Db as _, Reader},
    shared::Shared,
};

#[derive(Default)]
pub struct DbConn(Option<Reader>);

impl DbConn {
    pub async fn acquire<C: Components>(&mut self, shared: &Shared<C>) -> AuxResult<&Reader> {
        if self.0.is_none() {
            self.0 = Some(shared.db().reader().await?);
        }
        self.0
            .as_ref()
            .ok_or_else(|| Aux::internal("reader vanished right after acquire"))
    }

    /// Вернуть соединение в пул: стрим делает это до ожидания живых элементов.
    pub fn release(&mut self) {
        self.0 = None;
    }
}
