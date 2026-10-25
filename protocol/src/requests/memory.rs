//! Память: только чтение и восстановление; мутации — тулы модели.

use crate::requests::Response;
use eva::service;
use eva::time::Timestamp;

use crate::{
    errors,
    requests::Limit,
    types::{
        MemoryId,
        memory::{Archive, Kind, Record, Version, Visibility},
        turn::TurnView,
    },
};

#[service(export, result = Response)]
pub trait Memory {
    #[args(derive(Default))]
    fn list(
        kind: Option<Kind>,
        visibility: Option<Visibility>,
        /// По `created_at`.
        since: Option<Timestamp>,
        #[serde(default)] limit: Limit,
        after: Option<MemoryId>,
    ) -> ok! {
        /// Только текущие версии.
        found: Vec<Record>,
    };
    embed! {
        code!(Err => []);
    }

    #[errors(errors::memory::NotFound)]
    fn get(
        id: MemoryId,
        view: Option<TurnView>,
    ) -> ok! {
        record: Record,
        archive: Option<Archive>,
    };
    embed! {
        code!(Err => [NotFound]);
    }

    #[errors(errors::memory::NotFound)]
    fn history(
        id: MemoryId,
        view: Option<TurnView>,
    ) -> ok! {
        /// Цепочка от корня к текущей.
        versions: Vec<Version>,
    };
    embed! {
        code!(Err => [NotFound]);
    }

    #[flags(mutating)]
    #[errors(
        errors::memory::NotFound,
        errors::memory::NotArchived,
        errors::memory::Superseded
    )]
    fn restore(
        id: MemoryId,
    ) -> ok! {
        record: Record,
    };
    embed! {
        code!(Err => [NotFound, NotArchived, Superseded]);
    }
}
