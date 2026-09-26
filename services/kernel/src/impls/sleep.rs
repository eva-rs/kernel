use eva_core::service::sleep::Sleep;
use eva_protocol::requests::sleep::{phase_runs, runs};

use crate::{components::Components, handling::sleep as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Sleep for Session<C> { project! {
    fn runs() = h::Runs;
    fn phase_runs() = h::PhaseRuns;
}}
