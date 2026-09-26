//! Строка `surfaces` → `Surface`.

use eva_protocol::types::surface::Surface;

use crate::db::surfaces::SurfaceRow;

pub fn from_db(row: SurfaceRow) -> Surface {
    let SurfaceRow {
        id,
        name,
        module,
        rules,
        trusted,
        model,
        effort,
        thinking,
        compact_at,
        builtin,
        created_at,
    } = row;

    Surface {
        id,
        name,
        module,
        rules,
        trusted,
        model,
        effort,
        thinking,
        compact_at,
        builtin,
        created_at,
    }
}
