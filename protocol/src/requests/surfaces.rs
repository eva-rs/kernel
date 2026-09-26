//! Surface: модуль создаёт своё лицо сам, хозяин настраивает доверие.

use eva::data;

use crate::{
    errors,
    requests::{Limit, Patch},
    types::{Effort, ModelId, SurfaceId, Thinking, surface::Surface},
};

pub mod create {
    use super::*;

    #[data]
    pub struct Args {
        pub id: SurfaceId,
        /// Обязательно, кроме `builtin`.
        pub name: Option<String>,
        /// Prompt-блок канала; многострочный, проверяется на маркеры реестра.
        pub rules: Option<String>,
        /// Модуль рисует кнопки pending-запросов.
        #[serde(default)]
        pub ask: bool,
        /// Занять встроенный `local` вместо создания нового.
        #[serde(default)]
        pub builtin: bool,
    }

    #[data]
    pub struct Ok {
        pub surface: Surface,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        Taken(#[from] errors::surfaces::Taken),
        #[display("{_0}")]
        BadRules(#[from] errors::surfaces::BadRules),
    }

    code!(Err => [Taken, BadRules]);
}

pub mod get {
    use super::*;

    #[data]
    pub struct Args {
        pub surface: SurfaceId,
    }

    #[data]
    pub struct Ok {
        pub surface: Surface,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::surfaces::NotFound),
    }

    code!(Err => [NotFound]);
}

pub mod list {
    use super::*;

    #[data]
    #[derive(Default)]
    pub struct Args {
        #[serde(default)]
        pub limit: Limit,
        pub after: Option<SurfaceId>,
    }

    #[data]
    pub struct Ok {
        pub found: Vec<Surface>,
    }

    #[data(error, display("_"))]
    pub enum Err {}

    code!(Err => []);
}

pub mod update {
    use super::*;

    #[serde_with::apply(Patch => #[serde(default)])]
    #[data]
    #[derive(Default)]
    pub struct Update {
        pub name: Patch<String>,
        pub rules: Patch<Option<String>>,
        /// Состояние сессии, в базе не хранится.
        pub ask: Patch<bool>,
        /// Доверие и модель ставит только хозяин.
        pub trusted: Patch<Option<bool>>,
        pub model: Patch<Option<ModelId>>,
        pub effort: Patch<Option<Effort>>,
        pub thinking: Patch<Option<Thinking>>,
        pub compact_at: Patch<Option<f32>>,
    }

    #[data]
    pub struct Args {
        pub surface: SurfaceId,
        #[serde(default)]
        pub update: Update,
    }

    #[data]
    pub struct Ok {
        pub surface: Surface,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::surfaces::NotFound),
        #[display("{_0}")]
        Builtin(#[from] errors::surfaces::Builtin),
        #[display("{_0}")]
        BadRules(#[from] errors::surfaces::BadRules),
    }

    code!(Err => [NotFound, Builtin, BadRules]);
}
