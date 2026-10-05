//! Сегмент (`01 §1.1`) → тип протокола.

use eva_protocol::types::chat::Segment;

use crate::db::turns::SegmentRow;

pub fn segment_from_db(row: SegmentRow) -> Segment {
    Segment {
        id: row.id,
        chat: row.chat,
        seq: row.seq,
        reason: row.reason,
        started_at: row.started_at,
        ended_at: row.ended_at,
        summary: row.summary,
    }
}
