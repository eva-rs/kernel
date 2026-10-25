//! Файлы модуля. Пути относительны каталогу модуля; выход наружу — `Aux::Forbidden`.

use crate::requests::Response;
use eva::{data, service, time::Timestamp};

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

#[service(export, result = Response)]
pub trait Fs {
    #[errors(errors::fs::NotFound, errors::fs::IsDir)]
    fn read(
        path: String,
    ) -> ok! {
        data: Bytes,
    };
    embed! {
        code!(Err => [NotFound, IsDir]);
    }

    #[flags(mutating)]
    #[errors(errors::fs::NotFound, errors::fs::IsDir)]
    fn write(
        path: String,
        data: Bytes,
        /// Создать недостающие каталоги.
        #[serde(default)]
        create_dirs: bool,
    ) -> ok! {
        size: u64,
    };
    embed! {
        code!(Err => [NotFound, IsDir]);
    }

    #[args(derive(Default))]
    #[errors(errors::fs::NotFound, errors::fs::NotDir)]
    fn list(
        /// `""` — корень каталога модуля.
        #[serde(default)]
        path: String,
    ) -> ok! {
        entries: Vec<Entry>,
    };
    embed! {
        code!(Err => [NotFound, NotDir]);
    }

    #[errors(errors::fs::NotFound)]
    fn stat(
        path: String,
    ) -> ok! {
        entry: Entry,
    };
    embed! {
        code!(Err => [NotFound]);
    }

    #[flags(mutating)]
    #[errors(errors::fs::NotFound)]
    fn remove(
        path: String,
        /// Каталог — вместе с содержимым.
        #[serde(default)]
        recursive: bool,
    ) -> ok! {};
    embed! {
        code!(Err => [NotFound]);
    }
}
