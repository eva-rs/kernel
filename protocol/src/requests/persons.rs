//! Личности: резолв адреса поверхности, переименование, слияние, whitelist.

use crate::requests::Response;
use eva::service;

use crate::{
    errors,
    requests::Limit,
    types::{PersonId, SurfaceId, person::Person},
};

#[service(export, result = Response)]
pub trait Persons {
    #[flags(mutating)]
    fn resolve(
        surface: SurfaceId,
        external_id: String,
        handle: Option<String>,
        name: Option<String>,
    ) -> ok! {
        person: Person,
    };
    embed! {
        code!(Err => []);
    }

    #[args(derive(Default))]
    fn list(
        surface: Option<SurfaceId>,
        whitelisted: Option<bool>,
        #[serde(default)] limit: Limit,
        after: Option<PersonId>,
    ) -> ok! {
        found: Vec<Person>,
    };
    embed! {
        code!(Err => []);
    }

    #[flags(mutating)]
    #[errors(errors::persons::NotFound, errors::persons::System)]
    fn rename(
        person: PersonId,
        name: String,
    ) -> ok! {
        person: Person,
    };
    embed! {
        code!(Err => [NotFound, System]);
    }

    #[flags(mutating)]
    #[errors(
        errors::persons::NotFound,
        errors::persons::System,
        errors::persons::SelfMerge,
        errors::persons::MasterMerge
    )]
    fn merge(
        from: PersonId,
        into: PersonId,
        /// Подтверждает слияние, меняющее `master`.
        #[serde(default)]
        confirm: bool,
    ) -> ok! {
        person: Person,
    };
    embed! {
        code!(Err => [NotFound, System, SelfMerge, MasterMerge]);
    }

    #[flags(mutating)]
    #[errors(
        errors::persons::NotFound,
        errors::persons::System,
        errors::persons::MasterWhitelist
    )]
    fn whitelist(
        person: PersonId,
        on: bool,
    ) -> ok! {
        person: Person,
    };
    embed! {
        code!(Err => [NotFound, System, MasterWhitelist]);
    }
}
