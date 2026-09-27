//! Blob-store: байты вложений ходят потоками, не в `Args`.

use eva::data;

use crate::{
    errors,
    requests::{ByteStream, Items},
    types::{
        BlobId, Bytes, ChatId,
        blob::{Kind, Meta},
    },
};

pub mod put {
    use super::*;

    /// По проводу — `Meta`, тело — поток чанков транспорта.
    #[data(not(Debug, serde, schemars, Clone, PartialEq))]
    pub struct Args {
        pub meta: Meta,
        pub body: ByteStream,
    }

    #[data]
    pub struct Meta {
        pub chat: ChatId,
        pub name: String,
        pub media_type: String,
        pub kind: Kind,
        pub size: u64,
        /// blake3 hex; сверяется по ходу приёма.
        pub hash: Option<String>,
    }

    #[data]
    pub struct Ok {
        pub id: BlobId,
        pub size: u64,
        pub hash: String,
    }

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        BadName(#[from] errors::blobs::BadName),
        #[display("{_0}")]
        SizeMismatch(#[from] errors::blobs::SizeMismatch),
        #[display("{_0}")]
        HashMismatch(#[from] errors::blobs::HashMismatch),
    }

    code!(Err => [BadName, SizeMismatch, HashMismatch]);
}

pub mod get {
    use super::*;

    #[data]
    pub struct Args {
        pub id: BlobId,
        /// Докачка с байта.
        #[serde(default)]
        pub offset: u64,
    }

    /// Сначала один `Meta`, затем чанки.
    #[data]
    pub enum Item {
        Meta {
            name: String,
            media_type: String,
            size: u64,
        },
        Chunk(Bytes),
    }

    pub type Ok = Items<Item, Err>;

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::blobs::NotFound),
    }

    code!(Err => [NotFound]);
}

pub mod stat {
    use super::*;

    #[data]
    pub struct Args {
        pub id: BlobId,
    }

    /// Не `#[serde(flatten)]`: плоская карта идёт через буфер serde, где `Timestamp` из msgpack не разбирается.
    pub type Ok = Meta;

    #[data(error)]
    pub enum Err {
        #[display("{_0}")]
        NotFound(#[from] errors::blobs::NotFound),
    }

    code!(Err => [NotFound]);
}
