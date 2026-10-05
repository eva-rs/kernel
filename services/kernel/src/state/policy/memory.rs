//! Память: только рука хозяина (`03 §7`); `Caller::Service` доступа не имеет,
//! клиенту private не фильтруется.

use eva_protocol::errors::AuxResult;

use crate::state::policy::Policy;

#[derive(Debug, Clone, Copy)]
pub struct MemoryPolicy<'a>(Policy<'a>);

impl<'a> Policy<'a> {
    pub const fn memory(self) -> MemoryPolicy<'a> {
        MemoryPolicy(self)
    }
}

impl MemoryPolicy<'_> {
    /// `memory.list/get/history`.
    pub fn read(self) -> AuxResult<()> {
        self.0.owner()
    }

    /// `memory.restore`: единственная мутация памяти вне тулов модели.
    pub fn restore(self) -> AuxResult<()> {
        self.0.owner()
    }
}
