//! Реестр моделей.

use crate::requests::Response;
use eva::service;

use crate::types::model::ModelEntry;

#[service(result = Response)]
pub trait Models {
    #[args(derive(Default))]
    fn list() -> ok! {
        models: Vec<ModelEntry>,
    };
    embed! {
        code!(Err => []);
    }
}
