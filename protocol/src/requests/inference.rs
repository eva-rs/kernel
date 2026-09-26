//! Разовый вызов модели модулем: без чата, истории и тулов.

use eva::data;
use serde_json::Value;

use crate::{
    errors,
    types::{
        Effort, ModelId,
        content::Block,
        generate::{Stop, Usage},
    },
};

pub mod complete {
    use super::*;

    #[data]
    pub struct Args {
        /// `None` — `llm.roles.cheap`.
        pub model: Option<ModelId>,
        pub system: String,
        pub input: Vec<Block>,
        /// JSON Schema структурированного ответа.
        pub schema: Option<Value>,
        pub max_tokens: Option<u32>,
        /// `None` — `low`.
        pub effort: Option<Effort>,
    }

    #[data]
    pub struct Ok {
        pub text: Option<String>,
        /// При заданной `schema`.
        pub json: Option<Value>,
        pub stop: Stop,
        pub usage: Usage,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        Provider(#[from] errors::inference::Provider),
    }
}
