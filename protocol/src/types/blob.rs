//! Blob-store: файлы и картинки вне контекста.

use eva::{data, time::Timestamp};

#[data(copy, ord)]
pub enum Kind {
    File,
    Image,
}

#[data]
pub struct Meta {
    /// Leaf без разделителей и `..`.
    pub name: String,
    pub media_type: String,
    pub size: u64,
    /// blake3 содержимого, hex.
    pub hash: String,
    pub kind: Kind,
    pub expires_at: Timestamp,
}
