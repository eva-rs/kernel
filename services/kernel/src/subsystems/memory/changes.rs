//! Изменение core и поправки открытых контекстов коммитятся вместе.

use super::{hot, visibility::Gate};
use crate::db::{DbError, Queries as _, Tx, stats::DayZone};
use crate::subsystems::turns::log::{self, Append, MemoryUpdatedPayload, Payload};
use eva::time::Timestamp;
use eva_protocol::types::{
    MemoryId,
    memory::{Kind, Visibility},
};
use std::collections::BTreeMap;

pub fn track<T, E: From<DbError>>(
    tx: &Tx<'_>,
    now: Timestamp,
    zone: &DayZone,
    change: impl FnOnce(&Tx<'_>) -> Result<T, E>,
) -> Result<T, E> {
    let snapshot = |tx: &Tx<'_>| -> Result<BTreeMap<MemoryId, (Visibility, String)>, DbError> {
        Ok(tx
            .memory()
            .current()?
            .into_iter()
            .filter(|r| r.kind == Kind::Core)
            .map(|r| (r.id, (r.visibility, hot::line(&r, now, zone))))
            .collect())
    };
    let before = snapshot(tx)?;
    let result = change(tx)?;
    let after = snapshot(tx)?;
    if before == after {
        return Ok(result);
    }
    for segment in tx.turns().open_segments()? {
        let Some(chat) = tx.chats().get(segment.chat)? else {
            continue;
        };
        if chat.deleted_at.is_some() {
            continue;
        }
        let gate = Gate::Hot {
            trusted_chat: chat.trusted_chat,
            audience: tx.turns().audience(chat.id)?,
        };
        let removed = before
            .iter()
            .filter(|(id, value)| after.get(id) != Some(value) && gate.allows(value.0))
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        let added = after
            .iter()
            .filter(|(id, value)| before.get(id) != Some(value) && gate.allows(value.0))
            .map(|(_, (_, line))| line.clone())
            .collect::<Vec<_>>();
        if added.is_empty() && removed.is_empty() {
            continue;
        }
        log::append(
            tx,
            Append {
                chat: chat.id,
                segment: segment.id,
                turn_id: None,
                step: None,
                now,
            },
            &Payload::MemoryUpdated(MemoryUpdatedPayload { added, removed }),
        )?;
    }
    Ok(result)
}
