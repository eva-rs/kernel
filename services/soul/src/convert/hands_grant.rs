//! Грант рук (`07 §3.1`) → тип протокола.

use eva_protocol::types::hands::Grant;

use crate::db::hands_grants::GrantRow;

pub fn from_db(row: GrantRow) -> Grant {
    let GrantRow {
        id,
        endpoint,
        surface,
        chat,
        kind,
        prefix,
        issued_by,
        created_at,
    } = row;

    Grant {
        id,
        endpoint,
        surface,
        chat,
        kind,
        prefix,
        issued_by,
        created_at,
    }
}
