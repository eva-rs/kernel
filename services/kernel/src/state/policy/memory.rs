//! Память: только рука хозяина; клиенту private не фильтруется.

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
    pub fn read(self) -> AuxResult<()> {
        self.0.owner()
    }
}
