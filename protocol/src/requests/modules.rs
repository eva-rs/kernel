//! Модули, реестр сервисов и токены клиентов.

use eva::data;

use crate::{
    errors,
    types::{
        ModuleId, PersonId,
        module::{Module, ServiceAddress, ServiceKey},
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

    code!(Err => []);
}

pub mod add {
    use super::*;

    #[data]
    pub struct Args {
        pub module: ModuleId,
        pub address: ServiceAddress,
        /// `None` — ключ генерирует ядро.
        pub key: Option<ServiceKey>,
    }

    #[data]
    pub struct Ok {
        /// Сгенерированный ядром ключ; показывается один раз.
        pub key: Option<ServiceKey>,
    }

    /// Зарезервированное или невалидное имя, негодный ключ — `Aux::Invalid`;
    /// адрес, который уже набирает другая запись реестра, — `Aux::Conflict`.
    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        Taken(#[from] errors::modules::Taken),
    }

    code!(Err => [Taken]);
}

pub mod remove {
    use super::*;

    #[data]
    pub struct Args {
        pub module: ModuleId,
    }

    #[data]
    pub struct Ok {}

    /// Запись `modules.services` — `Aux::Conflict`.
    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        ServiceNotFound(#[from] errors::modules::ServiceNotFound),
    }

    code!(Err => [ServiceNotFound]);
}

pub mod token_issue {
    use super::*;

    #[data]
    pub struct Args {
        pub module: ModuleId,
        /// Заполняет только обёртка тула: актор тура.
        pub issued_by: Option<PersonId>,
    }

    #[data]
    pub struct Ok {
        /// Токен клиента; показывается один раз, ядро хранит только хэш.
        pub token: String,
    }

    /// Зарезервированное или невалидное имя — `Aux::Invalid`.
    #[data(error, display("_"))]
    pub enum Err {}

    code!(Err => []);
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

    code!(Err => [NotFound]);
}
