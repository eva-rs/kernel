//! SQLite-файл модуля. `ATTACH` — `Aux::Forbidden`, перебор строк — `Aux::Quota`.

use eva::data;
use serde_json::Value;

use crate::errors;

#[data]
pub struct Statement {
    pub sql: String,
    #[serde(default)]
    pub params: Vec<Value>,
}

pub mod exec {
    use super::*;

    #[data]
    pub struct Args {
        #[serde(flatten)]
        pub statement: Statement,
    }

    #[data]
    pub struct Ok {
        pub changes: u64,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        Sql(#[from] errors::sqlite::Sql),
    }

    code!(Err => [Sql]);
}

pub mod query {
    use super::*;

    #[data]
    pub struct Args {
        #[serde(flatten)]
        pub statement: Statement,
    }

    #[data]
    pub struct Ok {
        pub columns: Vec<String>,
        pub rows: Vec<Vec<Value>>,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        Sql(#[from] errors::sqlite::Sql),
    }

    code!(Err => [Sql]);
}

pub mod tx {
    use super::*;

    /// Одна транзакция: всё или ничего.
    #[data]
    pub struct Args {
        pub statements: Vec<Statement>,
    }

    #[data]
    pub struct Ok {
        /// По оператору.
        pub changes: Vec<u64>,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        Sql(#[from] errors::sqlite::Sql),
    }

    code!(Err => [Sql]);
}
