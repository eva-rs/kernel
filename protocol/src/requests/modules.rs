//! Модули, реестр сервисов и токены клиентов.

use crate::requests::Response;
use eva::service;

use crate::{
    errors,
    types::{
        ModuleId, PersonId,
        module::{Module, ServiceAddress, ServiceKey},
    },
};

#[service(result = Response)]
pub trait Modules {
    #[args(derive(Default))]
    fn list() -> ok! {
        modules: Vec<Module>,
    };
    embed! {
        code!(Err => []);
    }

    #[flags(mutating)]
    #[errors(errors::modules::Taken)]
    fn add(
        module: ModuleId,
        address: ServiceAddress,
        /// `None` — ключ генерирует ядро.
        key: Option<ServiceKey>,
    ) -> ok! {
        /// Сгенерированный ядром ключ; показывается один раз.
        key: Option<ServiceKey>,
    };
    embed! {
        code!(Err => [Taken]);
    }

    #[flags(mutating)]
    #[errors(errors::modules::ServiceNotFound)]
    fn remove(module: ModuleId) -> ok! {};
    embed! {
        code!(Err => [ServiceNotFound]);
    }

    #[flags(mutating)]
    fn token_issue(
        module: ModuleId,
        /// Заполняет только обёртка тула: актор тура.
        issued_by: Option<PersonId>,
    ) -> ok! {
        /// Токен клиента; показывается один раз, ядро хранит только хэш.
        token: String,
    };
    embed! {
        code!(Err => []);
    }

    #[flags(mutating)]
    #[errors(errors::modules::NotFound)]
    fn token_revoke(module: ModuleId) -> ok! {};
    embed! {
        code!(Err => [NotFound]);
    }
}
