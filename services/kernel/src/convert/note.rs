//! Заметка чата (`04`) → тип протокола.

use eva_protocol::types::note::Note;

use crate::db::notes::NoteRow;

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
