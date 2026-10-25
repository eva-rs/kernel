//! Папки: CRUD хозяина, наследование trust и модели.

use crate::requests::Response;
use eva::{data, service};

use crate::{
    errors,
    requests::{Limit, Patch},
    types::{Effort, FolderId, ModelId, SurfaceId, Thinking, folder::Folder},
};

#[service(result = Response)]
pub trait Folders {
    #[flags(mutating)]
    #[errors(errors::folders::NameTaken)]
    fn create(
        surface: SurfaceId,
        name: String,
    ) -> ok! {
        folder: Folder,
    };
    embed! {
        code!(Err => [NameTaken]);
    }

    #[flags(mutating)]
    #[errors(
        errors::folders::NotFound,
        errors::folders::Builtin,
        errors::folders::NameTaken
    )]
    fn update(
        folder: FolderId,
        #[serde(default)] update: this![Update],
    ) -> ok! {
        folder: Folder,
    };
    embed! {
        #[serde_with::apply(Patch => #[serde(default)])]
        #[data]
        #[derive(Default)]
        pub struct Update {
            pub name: Patch<String>,
            pub trusted: Patch<Option<bool>>,
            pub model: Patch<Option<ModelId>>,
            pub effort: Patch<Option<Effort>>,
            pub thinking: Patch<Option<Thinking>>,
            pub compact_at: Patch<Option<f32>>,
        }

        code!(Err => [NotFound, Builtin, NameTaken]);
    }

    #[flags(mutating)]
    #[errors(errors::folders::NotFound, errors::folders::Builtin)]
    fn delete(
        folder: FolderId,
    ) -> ok! {
        /// Сколько чатов уехало в `Unsorted`.
        moved: u64,
    };
    embed! {
        code!(Err => [NotFound, Builtin]);
    }

    #[args(derive(Default))]
    fn list(
        surface: Option<SurfaceId>,
        #[serde(default)] limit: Limit,
        after: Option<FolderId>,
    ) -> ok! {
        found: Vec<Folder>,
    };
    embed! {
        code!(Err => []);
    }
}
