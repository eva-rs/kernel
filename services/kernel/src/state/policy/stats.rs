//! Статистика: глобальная — только хозяину; `stats.chat_spend` решает
//! `chats().read`.

use eva_protocol::errors::AuxResult;

use crate::state::policy::Policy;

#[derive(Debug, Clone, Copy)]
pub struct StatsPolicy<'a>(Policy<'a>);

impl<'a> Policy<'a> {
    pub const fn stats(self) -> StatsPolicy<'a> {
        StatsPolicy(self)
    }
}

impl StatsPolicy<'_> {
    /// `stats.*`, кроме `chat_spend`.
    pub fn global(self) -> AuxResult<()> {
        self.0.owner()
    }
}
