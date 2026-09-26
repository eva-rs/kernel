//! Запись памяти и архив (`03`) → тип протокола.

use eva_protocol::types::memory::{Archive, Provenance, Record, Version};

use crate::db::memory::{ArchiveRow, MemoryRow};

pub fn record(row: MemoryRow) -> (Record, Option<Archive>) {
    let MemoryRow {
        id,
        kind,
        text,
        name,
        description,
        tags,
        visibility,
        importance,
        confidence,
        source_actor,
        source_chat,
        source_surface,
        source_trusted_chat,
        source_run,
        created_at,
        updated_at,
        consolidated_at,
        occurred_at,
        valid_from,
        valid_to,
        supersedes,
        version,
        last_reinforced_at,
        access_count,
        embed_model,
        archive,
    } = row;

    let record = Record {
        id,
        kind,
        text,
        name,
        description,
        tags: tags
            .as_deref()
            .map(|t| t.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default(),
        visibility,
        importance: importance as f32,
        confidence: confidence as f32,
        source: Provenance {
            actor: source_actor,
            chat: source_chat,
            surface: source_surface,
            trusted_chat: source_trusted_chat,
            run: source_run,
        },
        created_at,
        updated_at,
        consolidated_at,
        occurred_at,
        valid_from,
        valid_to,
        supersedes,
        version,
        last_reinforced_at,
        access_count: access_count as f32,
        embed_model,
    };
    (record, archive.map(self::archive))
}

pub fn archive(row: ArchiveRow) -> Archive {
    let ArchiveRow {
        reason,
        why,
        run,
        actor,
        archived_at,
        restored_at,
    } = row;

    Archive {
        reason,
        why,
        run,
        actor,
        archived_at,
        restored_at,
    }
}

pub fn version(row: MemoryRow) -> Version {
    let (record, archive) = record(row);
    Version { record, archive }
}
