//! Личности: резолв адреса поверхности, переименование, слияние, whitelist.

use eva::data;

use crate::{
    errors,
    requests::Limit,
    types::{PersonId, SurfaceId, person::Person},
};

pub mod resolve {
    use super::*;

    #[data]
    pub struct Args {
        pub surface: SurfaceId,
        pub external_id: String,
        pub handle: Option<String>,
        pub name: Option<String>,
    }

    #[data]
    pub struct Ok {
        pub person: Person,
    }

    #[data(error, display("_"))]
    pub enum Err {}
}

pub mod list {
    use super::*;

    #[data]
    #[derive(Default)]
    pub struct Args {
        pub surface: Option<SurfaceId>,
        pub whitelisted: Option<bool>,
        #[serde(default)]
        pub limit: Limit,
        pub after: Option<PersonId>,
    }

    #[data]
    pub struct Ok {
        pub found: Vec<Person>,
    }

    #[data(error, display("_"))]
    pub enum Err {}
}

pub mod rename {
    use super::*;

    #[data]
    pub struct Args {
        pub person: PersonId,
        pub name: String,
    }

    #[data]
    pub struct Ok {
        pub person: Person,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::persons::NotFound),
        #[display("{_0}")]
        System(#[from] errors::persons::System),
    }
}

pub mod merge {
    use super::*;

    #[data]
    pub struct Args {
        pub from: PersonId,
        pub into: PersonId,
        /// Подтверждает слияние, меняющее `master`.
        #[serde(default)]
        pub confirm: bool,
    }

    #[data]
    pub struct Ok {
        pub person: Person,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::persons::NotFound),
        #[display("{_0}")]
        System(#[from] errors::persons::System),
        #[display("{_0}")]
        SelfMerge(#[from] errors::persons::SelfMerge),
        #[display("{_0}")]
        MasterMerge(#[from] errors::persons::MasterMerge),
    }
}

pub mod whitelist {
    use super::*;

    #[data]
    pub struct Args {
        pub person: PersonId,
        pub on: bool,
    }

    #[data]
    pub struct Ok {
        pub person: Person,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::persons::NotFound),
        #[display("{_0}")]
        System(#[from] errors::persons::System),
        #[display("{_0}")]
        MasterWhitelist(#[from] errors::persons::MasterWhitelist),
    }
}
