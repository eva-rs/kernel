//! Учёт тура (`09 §1.3`): бакеты `tool_stats`/`chat_stats` пишет только
//! `TurnTally` одной транзакцией на выходе из тура.

use std::collections::BTreeMap;

use eva::time::Timestamp;

use eva_protocol::types::{ChatId, ToolId};

use crate::{
    components::Components,
    db::{
        Db as _, DbResult, Queries as _,
        stats::{ChatDelta, ToolDelta},
    },
    shared::Shared,
};

/// диспетчер тулов зовёт `note_*`, движок туров — `finish`.
/// Брошенный без `finish` (отмена future, паника) сбрасывается фоном как `cancelled`.
pub struct TurnTally<C: Components> {
    shared: Shared<C>,
    chat: ChatId,
    started_at: Timestamp,
    tools: BTreeMap<ToolId, ToolDelta>,
    delta: ChatDelta,
    finished: bool,
}

impl<C: Components> TurnTally<C> {
    pub(super) fn new(shared: Shared<C>, chat: ChatId, started_at: Timestamp) -> Self {
        Self {
            shared,
            chat,
            started_at,
            tools: BTreeMap::new(),
            delta: ChatDelta::default(),
            finished: false,
        }
    }

    /// Исполненный вызов (прошёл I3); тур закрывает только удачный.
    pub fn note_call(&mut self, tool: ToolId, is_error: bool, ends_turn: bool) {
        let d = self.tools.entry(tool).or_default();
        d.calls += 1;
        if is_error {
            d.errors += 1;
        } else if ends_turn {
            d.ends_turn += 1;
        }
    }

    /// Удачный `tools_load` из резерва.
    pub fn note_load(&mut self, tool: ToolId) {
        self.tools.entry(tool).or_default().loads += 1;
    }

    pub fn note_step(&mut self) {
        self.delta.steps += 1;
    }

    pub fn note_user_message(&mut self) {
        self.delta.user_messages += 1;
    }

    pub fn note_assistant_message(&mut self) {
        self.delta.assistant_messages += 1;
    }

    /// Тур кончился любым исходом: `turns += 1`, отмена — ещё и `cancelled`.
    pub async fn finish(mut self, cancelled: bool) -> DbResult<()> {
        self.finished = true;
        let (shared, flush) = self.take(cancelled);
        flush.write(&shared).await
    }

    fn take(&mut self, cancelled: bool) -> (Shared<C>, Flush) {
        let mut delta = std::mem::take(&mut self.delta);
        delta.turns = 1;
        delta.cancelled = u32::from(cancelled);
        let flush = Flush {
            chat: self.chat,
            started_at: self.started_at,
            tools: std::mem::take(&mut self.tools).into_iter().collect(),
            delta,
        };
        (self.shared.clone(), flush)
    }
}

impl<C: Components> Drop for TurnTally<C> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let (shared, flush) = self.take(true);
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn(async move {
                    if let Err(e) = flush.write(&shared).await {
                        log::error!(chat:% = flush.chat, error:% = e; "failed to flush stats of a dropped turn");
                    }
                });
            }
            Err(_) => log::warn!(chat:% = flush.chat; "turn stats lost: no runtime to flush them"),
        }
    }
}

struct Flush {
    chat: ChatId,
    started_at: Timestamp,
    tools: Vec<(ToolId, ToolDelta)>,
    delta: ChatDelta,
}

impl Flush {
    async fn write<C: Components>(&self, shared: &Shared<C>) -> DbResult<()> {
        let zone = shared.day_zone();
        let (chat, started_at, tools, delta) =
            (self.chat, self.started_at, self.tools.clone(), self.delta);
        shared
            .db()
            .write(move |tx| {
                tx.stats()
                    .flush_turn(chat, started_at, &tools, &delta, &zone)
            })
            .await
    }
}
