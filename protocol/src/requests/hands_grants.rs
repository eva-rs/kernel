//! Постоянные разрешения рук.

use crate::requests::Response;
use eva::service;

use crate::{
    errors,
    types::{ChatId, HandsGrantId, SurfaceId, hands::Grant},
};

#[service(result = Response)]
pub trait HandsGrants {
    #[args(derive(Default))]
    fn list(
        endpoint: Option<String>,
        surface: Option<SurfaceId>,
        chat: Option<ChatId>,
    ) -> ok! {
        grants: Vec<Grant>,
    };
    embed! {
        code!(Err => []);
    }

    #[flags(mutating)]
    #[errors(errors::hands_grants::NotFound)]
    fn revoke(id: HandsGrantId) -> ok! {};
    embed! {
        code!(Err => [NotFound]);
    }
}
