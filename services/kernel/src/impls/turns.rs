use eva_core::service::turns::Turns;
use eva_protocol::requests::turns::{append_silent, attach, cancel, delivered, respond, start};

use crate::{components::Components, handling::turns as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Turns for Session<C> { project! {
    fn start() = h::Start;
    stream fn attach() = h::Attach;
    fn append_silent() = h::AppendSilent;
    fn delivered() = h::Delivered;
    fn cancel() = h::Cancel;
    fn respond() = h::Respond;
}}
