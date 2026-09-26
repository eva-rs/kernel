//! Реестр моделей.

use eva::data;

use crate::types::model::ModelEntry;

pub mod list {
    use super::*;

    #[data]
    #[derive(Default)]
    pub struct Args {}

    #[data]
    pub struct Ok {
        pub models: Vec<ModelEntry>,
    }

    #[data(error, display("_"))]
    pub enum Err {}

    code!(Err => []);
}
