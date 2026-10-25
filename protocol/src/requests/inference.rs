//! Разовый вызов модели модулем: без чата, истории и тулов.

use crate::requests::Response;
use eva::service;

use serde_json::Value;

use crate::{
    errors,
    types::{
        Effort, ModelId,
        content::Block,
        generate::{Stop, Usage},
    },
};

#[service(export, result = Response)]
pub trait Inference {
    #[flags(mutating)]
    #[errors(errors::inference::Provider)]
    fn complete(
        /// `None` — `llm.roles.cheap`.
        model: Option<ModelId>,
        system: String,
        input: Vec<Block>,
        /// JSON Schema структурированного ответа.
        schema: Option<Value>,
        max_tokens: Option<u32>,
        /// `None` — `low`.
        effort: Option<Effort>,
    ) -> ok! {
        text: Option<String>,
        /// При заданной `schema`.
        json: Option<Value>,
        stop: Stop,
        usage: Usage,
    };
    embed! {
        code!(Err => [Provider]);
    }
}
