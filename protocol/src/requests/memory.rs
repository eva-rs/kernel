//! Память: только чтение и восстановление; мутации — тулы модели.

use eva::{data, time::Timestamp};

use crate::{
    errors,
    requests::Limit,
    types::{
        MemoryId,
        memory::{Archive, Kind, Record, Version, Visibility},
        turn::TurnView,
    },
};

pub mod list {
    use super::*;

    #[data]
    #[derive(Default)]
    pub struct Args {
        pub kind: Option<Kind>,
        pub visibility: Option<Visibility>,
        /// По `created_at`.
        pub since: Option<Timestamp>,
        #[serde(default)]
        pub limit: Limit,
        pub after: Option<MemoryId>,
    }

    #[data]
    pub struct Ok {
        /// Только текущие версии.
        pub found: Vec<Record>,
    }

    #[data(error, display("_"))]
    pub enum Err {}
}

pub mod get {
    use super::*;

    #[data]
    pub struct Args {
        pub id: MemoryId,
        pub view: Option<TurnView>,
    }

    #[data]
    pub struct Ok {
        pub record: Record,
        pub archive: Option<Archive>,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::memory::NotFound),
    }
}

pub mod history {
    use super::*;

    #[data]
    pub struct Args {
        pub id: MemoryId,
        pub view: Option<TurnView>,
    }

    #[data]
    pub struct Ok {
        /// Цепочка от корня к текущей.
        pub versions: Vec<Version>,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::memory::NotFound),
    }
}

pub mod restore {
    use super::*;

    #[data]
    pub struct Args {
        pub id: MemoryId,
    }

    #[data]
    pub struct Ok {
        pub record: Record,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::memory::NotFound),
        #[display("{_0}")]
        NotArchived(#[from] errors::memory::NotArchived),
        #[display("{_0}")]
        Superseded(#[from] errors::memory::Superseded),
    }
}
