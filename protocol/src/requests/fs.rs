//! Файлы модуля. Пути относительны каталогу модуля; выход наружу — `Aux::Forbidden`.

use eva::{data, time::Timestamp};

use crate::{errors, types::Bytes};

#[data(copy, ord)]
pub enum EntryKind {
    File,
    Dir,
}

#[data]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Timestamp,
}

pub mod read {
    use super::*;

    #[data]
    pub struct Args {
        pub path: String,
    }

    #[data]
    pub struct Ok {
        pub data: Bytes,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::fs::NotFound),
        #[display("{_0}")]
        IsDir(#[from] errors::fs::IsDir),
    }

    code!(Err => [NotFound, IsDir]);
}

pub mod write {
    use super::*;

    #[data]
    pub struct Args {
        pub path: String,
        pub data: Bytes,
        /// Создать недостающие каталоги.
        #[serde(default)]
        pub create_dirs: bool,
    }

    #[data]
    pub struct Ok {
        pub size: u64,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::fs::NotFound),
        #[display("{_0}")]
        IsDir(#[from] errors::fs::IsDir),
    }

    code!(Err => [NotFound, IsDir]);
}

pub mod list {
    use super::*;

    #[data]
    #[derive(Default)]
    pub struct Args {
        /// `""` — корень каталога модуля.
        #[serde(default)]
        pub path: String,
    }

    #[data]
    pub struct Ok {
        pub entries: Vec<Entry>,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::fs::NotFound),
        #[display("{_0}")]
        NotDir(#[from] errors::fs::NotDir),
    }

    code!(Err => [NotFound, NotDir]);
}

pub mod stat {
    use super::*;

    #[data]
    pub struct Args {
        pub path: String,
    }

    #[data]
    pub struct Ok {
        pub entry: Entry,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::fs::NotFound),
    }

    code!(Err => [NotFound]);
}

pub mod remove {
    use super::*;

    #[data]
    pub struct Args {
        pub path: String,
        /// Каталог — вместе с содержимым.
        #[serde(default)]
        pub recursive: bool,
    }

    #[data]
    pub struct Ok {}

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::fs::NotFound),
    }

    code!(Err => [NotFound]);
}
