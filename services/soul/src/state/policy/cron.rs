//! Крон: операции только хозяину — `owner()`.

use super::Policy;
use eva_protocol::{
    errors::{Aux, AuxResult},
    types::turn::TurnView,
};

impl Policy<'_> {
    pub fn cron_tool(view: &TurnView) -> AuxResult<()> {
        if view.master_turn && view.trusted_chat {
            Ok(())
        } else {
            Err(Aux::forbidden("cron tools require a trusted master turn"))
        }
    }
}
