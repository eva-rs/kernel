//! Заметка чата (`04`) → тип протокола.

use eva_protocol::types::note::{Note, Snapshot};

use crate::db::notes::{NoteRow, SnapshotRow};

pub fn snapshot(row: SnapshotRow) -> Snapshot {
    Snapshot {
        id: row.id,
        created_at: row.created_at,
        reason: row.reason,
        count: row.count,
    }
}

pub fn from_db(row: NoteRow) -> Note {
    let NoteRow {
        name,
        description,
        body,
        prev_body,
        pinned,
        created_at,
        updated_at,
    } = row;

    Note {
        name,
        description,
        body,
        prev_body,
        pinned,
        created_at,
        updated_at,
    }
}
