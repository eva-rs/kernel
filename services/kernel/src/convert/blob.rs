//! Blob (`07 §6`) → тип протокола.

use eva_protocol::types::blob::Meta;

use crate::db::blobs::BlobRow;

pub fn meta(row: BlobRow) -> Meta {
    let BlobRow {
        name,
        media_type,
        size,
        hash,
        kind,
        expires_at,
        ..
    } = row;

    Meta {
        name,
        media_type,
        size,
        hash: hex(&hash),
        kind,
        expires_at,
    }
}

/// Строчный hex: форма `hash` на проводе.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            _ = write!(out, "{b:02x}");
            out
        })
}
