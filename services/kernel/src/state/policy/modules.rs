//! Модули и токены: только хозяину; `issued_by` — поле обёртки тула.

use eva_protocol::{
    errors::{Aux, AuxResult},
    types::{ModuleId, PersonId},
};

use crate::state::policy::Policy;

#[derive(Debug, Clone, Copy)]
pub struct ModulesPolicy<'a>(Policy<'a>);

impl<'a> Policy<'a> {
    pub const fn modules(self) -> ModulesPolicy<'a> {
        ModulesPolicy(self)
    }
}

impl ModulesPolicy<'_> {
    /// `modules.token_issue`: личность выдавшего называет только ядро (актор
    /// тура); свой токен модуль не ротирует — ротация закрыла бы сессию,
    /// которой уходит единственная копия нового токена.
    pub fn issue(self, module: &ModuleId, issued_by: Option<&PersonId>) -> AuxResult<()> {
        self.0.owner()?;
        if issued_by.is_some() && !self.0.caller().is_kernel() {
            return Err(Aux::forbidden("issued_by is set by the kernel tool only"));
        }
        if self.0.caller().module() == Some(module) {
            return Err(Aux::Conflict(format!(
                "module {module} cannot rotate the token of its own session: issue it from another client or the kernel tool"
            )));
        }
        Ok(())
    }
}
