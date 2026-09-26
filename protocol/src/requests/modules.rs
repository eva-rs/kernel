//! Модули и их токены.

use eva::data;

use crate::{
    errors,
    types::{
        ModuleId, PersonId,
        module::{Module, TokenKind},
    },
};

pub mod list {
    use super::*;

    #[data]
    #[derive(Default)]
    pub struct Args {}

    #[data]
    pub struct Ok {
        pub modules: Vec<Module>,
    }

    #[data(error, display("_"))]
    pub enum Err {}
}

pub mod token_issue {
    use super::*;

    #[data]
    pub struct Args {
        pub module: ModuleId,
        pub kind: TokenKind,
        /// Заполняет только обёртка тула: актор тура.
        pub issued_by: Option<PersonId>,
    }

    #[data]
    pub struct Ok {
        /// Показывается один раз; ядро хранит только хэш.
        pub token: String,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        Reserved(#[from] errors::modules::Reserved),
    }
}

pub mod token_revoke {
    use super::*;

    #[data]
    pub struct Args {
        pub module: ModuleId,
    }

    #[data]
    pub struct Ok {}

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::modules::NotFound),
    }
}
