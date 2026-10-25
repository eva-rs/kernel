//! Подагенты.

use crate::requests::Response;
use eva::service;

use crate::types::{ChatId, agent::Agent};

#[service(export, result = Response)]
pub trait Agents {
    #[args(derive(Default))]
    fn list(
        /// Подагенты этого тимлида; `None` — все.
        chat: Option<ChatId>,
    ) -> ok! {
        agents: Vec<Agent>,
    };
    embed! {
        code!(Err => []);
    }
}
