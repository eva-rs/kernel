//! Постоянные разрешения рук.

use eva::data;

use crate::{
    errors,
    types::{ChatId, HandsGrantId, SurfaceId, hands::Grant},
};

pub mod list {
    use super::*;

    #[data]
    #[derive(Default)]
    pub struct Args {
        pub endpoint: Option<String>,
        pub surface: Option<SurfaceId>,
        pub chat: Option<ChatId>,
    }

    #[data]
    pub struct Ok {
        pub grants: Vec<Grant>,
    }

    #[data(error, display("_"))]
    pub enum Err {}
}

pub mod revoke {
    use super::*;

    #[data]
    pub struct Args {
        pub id: HandsGrantId,
    }

    #[data]
    pub struct Ok {}

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::hands_grants::NotFound),
    }
}
