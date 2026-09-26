use eva_core::service::stats::Stats;
use eva_protocol::requests::stats::{chat_spend, health, spend, spend_series, tools_usage};

use crate::{components::Components, handling::stats as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Stats for Session<C> { project! {
    fn tools_usage() = h::ToolsUsage;
    fn spend() = h::Spend;
    fn spend_series() = h::SpendSeries;
    fn chat_spend() = h::ChatSpend;
    fn health() = h::Health;
}}
