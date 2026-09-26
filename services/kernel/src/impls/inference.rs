use eva_core::service::inference::Inference;
use eva_protocol::requests::inference::complete;

use crate::{components::Components, handling::inference as h, session::Session};

#[rustfmt::skip]
impl<C: Components> Inference for Session<C> { project! {
    fn complete() = h::Complete;
}}
