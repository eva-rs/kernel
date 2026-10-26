//! Capability на handshake (`06 §3`).

use eva_protocol::errors::{Aux, AuxResult};

use crate::{config::LlmConfig, state::policy::Policy};

#[derive(Debug, Clone, Copy)]
pub struct CapabilitiesPolicy<'a>(Policy<'a>);

impl<'a> Policy<'a> {
    pub const fn capabilities(self) -> CapabilitiesPolicy<'a> {
        CapabilitiesPolicy(self)
    }
}

impl CapabilitiesPolicy<'_> {
    /// `provider` — только модулю, названному в `llm.providers` с `kind: module`:
    /// провайдер видит полный транскрипт, включая private-память.
    pub fn provider(self, llm: &LlmConfig) -> AuxResult<()> {
        let module = self.0.module()?;
        if llm.names_module(module.as_str()) {
            Ok(())
        } else {
            Err(Aux::forbidden(format_args!(
                "module {module} is not named in llm.providers with kind: module"
            )))
        }
    }
}
