//! KV модуля, last-write-wins.

use eva::data;
use serde_json::Value;

use crate::requests::Limit;

pub mod get {
    use super::*;

    #[data]
    pub struct Args {
        pub key: String,
    }

    #[data]
    pub struct Ok {
        pub value: Option<Value>,
    }

    #[data(error, display("_"))]
    pub enum Err {}

    code!(Err => []);
}

pub mod set {
    use super::*;

    #[data]
    pub struct Args {
        pub key: String,
        pub value: Value,
    }

    #[data]
    pub struct Ok {}

    #[data(error, display("_"))]
    pub enum Err {}

    code!(Err => []);
}

pub mod del {
    use super::*;

    #[data]
    pub struct Args {
        pub key: String,
    }

    #[data]
    pub struct Ok {}

    #[data(error, display("_"))]
    pub enum Err {}

    code!(Err => []);
}

pub mod list {
    use super::*;

    #[data]
    #[derive(Default)]
    pub struct Args {
        #[serde(default)]
        pub prefix: String,
        #[serde(default)]
        pub limit: Limit,
        /// Ключи по возрастанию, keyset по ключу.
        pub after: Option<String>,
    }

    #[data]
    pub struct Ok {
        pub keys: Vec<String>,
    }

    #[data(error, display("_"))]
    pub enum Err {}

    code!(Err => []);
}
