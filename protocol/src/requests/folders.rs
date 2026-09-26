//! Папки: CRUD хозяина, наследование trust и модели.

use eva::data;

use crate::{
    errors,
    requests::{Limit, Patch},
    types::{Effort, FolderId, ModelId, SurfaceId, Thinking, folder::Folder},
};

pub mod create {
    use super::*;

    #[data]
    pub struct Args {
        pub surface: SurfaceId,
        pub name: String,
    }

    #[data]
    pub struct Ok {
        pub folder: Folder,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NameTaken(#[from] errors::folders::NameTaken),
    }

    code!(Err => [NameTaken]);
}

pub mod update {
    use super::*;

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

    #[data]
    pub struct Args {
        pub folder: FolderId,
        #[serde(default)]
        pub update: Update,
    }

    #[data]
    pub struct Ok {
        pub folder: Folder,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::folders::NotFound),
        #[display("{_0}")]
        Builtin(#[from] errors::folders::Builtin),
        #[display("{_0}")]
        NameTaken(#[from] errors::folders::NameTaken),
    }

    code!(Err => [NotFound, Builtin, NameTaken]);
}

pub mod delete {
    use super::*;

    #[data]
    pub struct Args {
        pub folder: FolderId,
    }

    #[data]
    pub struct Ok {
        /// Сколько чатов уехало в `Unsorted`.
        pub moved: u64,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::folders::NotFound),
        #[display("{_0}")]
        Builtin(#[from] errors::folders::Builtin),
    }

    code!(Err => [NotFound, Builtin]);
}

pub mod list {
    use super::*;

    #[data]
    #[derive(Default)]
    pub struct Args {
        pub surface: Option<SurfaceId>,
        #[serde(default)]
        pub limit: Limit,
        pub after: Option<FolderId>,
    }

    #[data]
    pub struct Ok {
        pub found: Vec<Folder>,
    }

    #[data(error, display("_"))]
    pub enum Err {}

    code!(Err => []);
}
