//! Surface — сущность ядра: модуль её только создаёт, ядро помнит создателя,
//! хозяин настраивает доверие.

use crate::requests::Response;
use eva::data;
use eva::service;

use crate::{
    errors,
    requests::{Limit, Patch},
    types::{Effort, ModelId, SurfaceId, Thinking, surface::Surface},
};

#[service(export, result = Response)]
pub trait Surfaces {
    #[flags(mutating)]
    #[args(
        doc = " Идемпотентно для создателя: свой `id` отдаётся как есть, `name`/`rules`",
        doc = " меняет `update`."
    )]
    #[errors(errors::surfaces::Taken, errors::surfaces::BadRules)]
    fn create(
        id: SurfaceId,
        name: String,
        /// Prompt-блок канала; многострочный, проверяется на маркеры реестра.
        rules: Option<String>,
    ) -> ok! {
        surface: Surface,
    };
    embed! {
        code!(Err => [Taken, BadRules]);
    }

    #[errors(errors::surfaces::NotFound)]
    fn get(
        surface: SurfaceId,
    ) -> ok! {
        surface: Surface,
    };
    embed! {
        code!(Err => [NotFound]);
    }

    #[args(derive(Default))]
    fn list(
        #[serde(default)] limit: Limit,
        after: Option<SurfaceId>,
    ) -> ok! {
        found: Vec<Surface>,
    };
    embed! {
        code!(Err => []);
    }

    #[flags(mutating)]
    #[errors(
        errors::surfaces::NotFound,
        errors::surfaces::Builtin,
        errors::surfaces::BadRules
    )]
    fn update(
        surface: SurfaceId,
        #[serde(default)] update: this![Update],
    ) -> ok! {
        surface: Surface,
    };
    embed! {
        #[serde_with::apply(Patch => #[serde(default)])]
        #[data]
        #[derive(Default)]
        pub struct Update {
            pub name: Patch<String>,
            pub rules: Patch<Option<String>>,
            /// Доверие и модель ставит только хозяин.
            pub trusted: Patch<Option<bool>>,
            pub model: Patch<Option<ModelId>>,
            pub effort: Patch<Option<Effort>>,
            pub thinking: Patch<Option<Thinking>>,
            pub compact_at: Patch<Option<f32>>,
        }

        code!(Err => [NotFound, Builtin, BadRules]);
    }
}
