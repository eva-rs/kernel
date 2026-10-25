//! KV модуля, last-write-wins.

use crate::requests::Response;
use eva::service;

use crate::{requests::Limit, types::Value};

#[service(result = Response)]
pub trait Kv {
    fn get(
        key: String,
    ) -> ok! {
        value: Option<Value>,
    };
    embed! {
        code!(Err => []);
    }

    #[flags(mutating)]
    fn set(key: String, value: Value) -> ok! {};
    embed! {
        code!(Err => []);
    }

    #[flags(mutating)]
    fn del(key: String) -> ok! {};
    embed! {
        code!(Err => []);
    }

    #[args(derive(Default))]
    fn list(
        #[serde(default)] prefix: String,
        #[serde(default)] limit: Limit,
        /// Ключи по возрастанию, keyset по ключу.
        after: Option<String>,
    ) -> ok! {
        keys: Vec<String>,
    };
    embed! {
        code!(Err => []);
    }
}
