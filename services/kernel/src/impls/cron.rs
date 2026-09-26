use eva_core::service::cron::Cron;
use eva_protocol::requests::cron::{list, runs};

use crate::{components::Components, handling::cron as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Cron for Session<C> { project! {
    fn list() = h::List;
    fn runs() = h::Runs;
}}
