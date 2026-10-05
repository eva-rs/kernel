//! Запуск фоновых туров через тот же журнал, отмену и движок.

use super::{
    ctx, ingest,
    log::{self, EndedBy, Payload},
};
use crate::{
    components::Components,
    db::{Db as _, Queries as _},
    shared::Shared,
    subsystems::{self, cron, sleep, stats::Origin, tools::expose::Whitelist},
};
use eva::perfect_derive;
use eva_protocol::{
    errors::Aux,
    types::{
        ChatId, EventId, PersonId,
        content::Block,
        turn::{FeedEvent, TurnOutcome},
    },
};

#[perfect_derive(Clone)]
pub struct Driver<C: Components> {
    pub shared: Shared<C>,
}

impl<C: Components> Driver<C> {
    pub fn new(shared: Shared<C>) -> Self {
        Self { shared }
    }
}

impl<C: Components> subsystems::CronTurns for Driver<C> {
    async fn run(&self, req: cron::TurnRequest) -> cron::Outcome {
        match self.cron(req).await {
            Ok(output) => cron::Outcome::Ok { output },
            Err(e) => cron::Outcome::Failed {
                error: e.to_string(),
            },
        }
    }
}

impl<C: Components> Driver<C> {
    async fn cron(&self, req: cron::TurnRequest) -> Result<Option<String>, Aux> {
        let slot = self.shared.turns().slot(req.chat);
        let mut feed = slot.feed.subscribe();
        let mut entry = ctx::Entry::master(req.chat, PersonId::system());
        entry.overrides = ctx::Overrides {
            model: req.model,
            effort: req.effort,
            thinking: None,
        };
        entry.source = Origin::Cron;
        entry.run_id = Some(req.run);
        entry.whitelist = req.tools.map_or(Whitelist::Unrestricted, |ids| {
            Whitelist::Restricted(ids.into_iter().collect())
        });
        let appended = ingest::append(
            &self.shared,
            entry,
            ingest::Message {
                content: vec![Block::text(req.message)],
                external_id: None,
                reply_to_external_id: None,
                silent: false,
                machine: true,
            },
        )
        .await?;
        let turn = appended
            .turn
            .turn()
            .cloned()
            .ok_or_else(|| Aux::internal("cron did not start a turn"))?;
        loop {
            match feed.recv().await {
                Ok(msg) => {
                    if let FeedEvent::TurnDone {
                        turn: id,
                        outcome,
                        error,
                    } = msg.item.event
                        && id == turn
                    {
                        if outcome != TurnOutcome::Completed {
                            return Err(Aux::Unavailable(
                                error.unwrap_or_else(|| format!("turn ended: {outcome}")),
                            ));
                        }
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let id = turn.clone();
                    let events = self
                        .shared
                        .db()
                        .reader()
                        .await?
                        .run(move |c| c.turns().turn_events_after(&id, EventId(0)))
                        .await?;
                    let ended = log::decode_all(events)
                        .map_err(Aux::from)?
                        .into_iter()
                        .find_map(|e| {
                            if let Payload::TurnEnded(t) = e.payload {
                                Some(t)
                            } else {
                                None
                            }
                        });
                    if let Some(t) = ended {
                        if t.outcome != TurnOutcome::Completed {
                            return Err(Aux::Unavailable(
                                t.error
                                    .unwrap_or_else(|| format!("turn ended: {}", t.outcome)),
                            ));
                        }
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return Err(Aux::Unavailable("turn feed closed".into()));
                }
            }
        }
        let rows = self
            .shared
            .db()
            .reader()
            .await?
            .run(move |c| c.turns().turn_events_after(&turn, EventId(0)))
            .await?;
        let events = log::decode_all(rows).map_err(Aux::from)?;
        let text = events.iter().rev().find_map(|e| {
            if let Payload::Message(m) = &e.payload {
                (m.role == eva_protocol::types::chat::Role::Assistant)
                    .then(|| super::items::text(&m.blocks))
            } else {
                None
            }
        });
        let budget = (f64::from(req.output_max) * self.shared.cfg().context.ratio_default) as usize;
        Ok(text.map(|s| {
            let mut end = s.len().min(budget);
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            s[..end].to_owned()
        }))
    }
}

impl<C: Components> subsystems::TurnCancel for Driver<C> {
    async fn cancel(&self, chats: Vec<ChatId>) {
        let live: Vec<_> = chats
            .into_iter()
            .filter_map(|chat| self.shared.turns().get(chat).and_then(|slot| slot.live()))
            .collect();
        for turn in &live {
            turn.cancel(EndedBy::Gc);
        }
        for turn in live {
            turn.closed.cancelled().await;
        }
    }
}

impl<C: Components> subsystems::WarmSets for Driver<C> {
    async fn recompute(&self, now: eva::time::Timestamp) -> Result<sleep::WarmOutcome, String> {
        crate::subsystems::tools::warm::recompute(&self.shared, now)
            .await
            .map_err(|e| e.to_string())
    }
}
