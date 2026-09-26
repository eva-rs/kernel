//! Подагенты.

use eva::data;

use crate::types::{ChatId, agent::Agent};

pub mod list {
    use super::*;

    #[data]
    #[derive(Default)]
    pub struct Args {
        /// Подагенты этого тимлида; `None` — все.
        pub chat: Option<ChatId>,
    }

    #[data]
    pub struct Ok {
        pub agents: Vec<Agent>,
    }

    #[data(error, display("_"))]
    pub enum Err {}
}
