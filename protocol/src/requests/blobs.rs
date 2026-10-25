//! Blob-store: байты вложений ходят потоками, не в `Args`.

use crate::requests::Response;
use eva::data;
use eva::service;

use crate::{
    errors,
    requests::{ByteStream, Items},
    types::{
        BlobId, Bytes, ChatId,
        blob::{Kind, Meta},
    },
};

#[service(export, result = Response)]
pub trait Blobs {
    #[flags(upload)]
    #[args(
        doc = " По проводу — `Meta`, тело — поток чанков транспорта.",
        data(not(Debug, serde, schemars, Clone, PartialEq))
    )]
    #[errors(
        errors::blobs::BadName,
        errors::blobs::SizeMismatch,
        errors::blobs::HashMismatch
    )]
    fn put(
        meta: this![Meta],
        body: ByteStream,
    ) -> ok! {
        id: BlobId,
        size: u64,
        hash: String,
    };
    embed! {
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

        code!(Err => [BadName, SizeMismatch, HashMismatch]);
    }

    #[flags(stream)]
    #[errors(errors::blobs::NotFound)]
    fn get(
        id: BlobId,
        /// Докачка с байта.
        #[serde(default)]
        offset: u64,
    ) -> ok!(
        #[data(not(Debug, serde, schemars, Clone, PartialEq))]
        Items<this![Item], this![Err]>
    );
    embed! {
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

        code!(Err => [NotFound]);
    }

    #[errors(errors::blobs::NotFound)]
    fn stat(id: BlobId) -> ok!(Meta);
    embed! {
        code!(Err => [NotFound]);
    }
}
