//! Чат, alias (`05 §1.3`, `§1.4`) и реплика истории → типы протокола.

use eva_protocol::types::chat::{Chat, Message};

use crate::db::chats::{ChatRow, MessageRow};

pub fn from_db(row: ChatRow) -> Chat {
    let ChatRow {
        id,
        folder_id,
        surface,
        kind,
        title,
        external_title,
        alias,
        pinned,
        alerted,
        undeletable,
        deleted_at,
        model,
        effort,
        thinking,
        compact_at,
        trusted,
        trusted_chat,
        project,
        workdir,
        workdir_on,
        live_turn,
        agent: _,
        created_at,
        last_activity_at,
    } = row;

    Chat {
        id,
        folder_id,
        surface_id: surface.id,
        kind,
        title,
        external_title,
        alias,
        pinned,
        alerted,
        undeletable,
        deleted_at,
        model,
        effort,
        thinking,
        compact_at,
        trusted,
        trusted_chat,
        project,
        workdir,
        workdir_on,
        live_turn,
        created_at,
        last_activity_at,
    }
}

pub fn message_from_db(row: MessageRow) -> Message {
    let MessageRow {
        event_id,
        seq,
        role,
        sender,
        blocks,
        external_id,
        reply_to_external_id,
        turn,
        created_at,
    } = row;

    Message {
        id: event_id,
        seq,
        role,
        sender,
        blocks,
        external_id,
        reply_to_external_id,
        turn,
        created_at,
    }
}
