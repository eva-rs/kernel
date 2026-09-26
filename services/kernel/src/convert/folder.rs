//! Папка (`05 §1.2`) → тип протокола.

use eva_protocol::types::folder::Folder;

use crate::db::folders::FolderRow;

pub fn from_db(row: FolderRow) -> Folder {
    let FolderRow {
        id,
        surface,
        name,
        builtin,
        trusted,
        model,
        effort,
        thinking,
        compact_at,
        surface_trusted: _,
    } = row;

    Folder {
        id,
        surface_id: surface.id,
        name,
        builtin,
        trusted,
        model,
        effort,
        thinking,
        compact_at,
    }
}
