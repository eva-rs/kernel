//! Блокнот и его снимки доступны только хозяину.

use eva_protocol::errors::AuxResult;

use crate::state::policy::Policy;

#[derive(Debug, Clone, Copy)]
pub struct NotesPolicy<'a>(Policy<'a>);

impl<'a> Policy<'a> {
    pub const fn notes(self) -> NotesPolicy<'a> {
        NotesPolicy(self)
    }
}

impl NotesPolicy<'_> {
    pub fn read(self) -> AuxResult<()> {
        self.0.owner()
    }

    pub fn write(self) -> AuxResult<()> {
        self.0.owner()
    }
}
