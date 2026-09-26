//! Содержимое реплики: байты вложений живут в blob-store, в логе — только ссылки.

use eva::data;

use crate::types::BlobId;

#[data]
pub enum Block {
    Text { text: String },
    Image { blob: BlobId, media_type: String },
    File { blob: BlobId },
}

impl Block {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}
