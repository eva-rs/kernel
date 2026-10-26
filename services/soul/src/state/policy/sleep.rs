//! Сон: только хозяину — `owner()`.

use super::Policy;
use eva_protocol::errors::AuxResult;

impl Policy<'_> {
    pub fn sleep_control(self) -> AuxResult<()> {
        self.owner()
    }
}
