//! Фазы сна используют тот же журнал, снимок тулов, шаги и отмену, что обычный тур.

use crate::{
    components::Components,
    db::{
        Db as _, DbResult, Queries as _,
        turns::{NewMessage, NewSegment},
    },
    subsystems::{
        self,
        sleep::{self, memory::Phase},
        stats::Origin,
        tools::expose::Whitelist,
        turns::{
            compaction,
            ctx::{self, Entry, Overrides},
            driver::Driver,
            engine::Turn,
            log::{self, Append, EndedBy, MessagePayload, Payload, SegmentResetPayload},
            snapshot,
        },
    },
};
use eva_protocol::{
    errors::Aux,
    types::{
        PersonId,
        chat::{Role, SegmentReason},
        content::Block,
        sleep::Phase as Kind,
    },
};
use std::sync::Arc;

impl<C: Components> subsystems::SleepTurns<C> for Driver<C> {
    async fn memory(
        &self,
        turn: sleep::PhaseTurn,
        phase: Arc<tokio::sync::Mutex<Phase<C>>>,
    ) -> sleep::Outcome {
        self.phase(turn, Some(phase), None).await
    }
    async fn groups(
        &self,
        turn: sleep::PhaseTurn,
        phase: Arc<tokio::sync::Mutex<sleep::groups::Phase<C>>>,
    ) -> sleep::Outcome {
        self.phase(turn, None, Some(phase)).await
    }
}

impl<C: Components> Driver<C> {
    async fn phase(
        &self,
        request: sleep::PhaseTurn,
        phase: Option<Arc<tokio::sync::Mutex<Phase<C>>>>,
        group_phase: Option<Arc<tokio::sync::Mutex<sleep::groups::Phase<C>>>>,
    ) -> sleep::Outcome {
        match self.phase_inner(request, phase, group_phase).await {
            Ok(report) => sleep::Outcome::Done { report },
            Err(error) => sleep::Outcome::Failed {
                error: error.to_string(),
            },
        }
    }
    async fn phase_inner(
        &self,
        request: sleep::PhaseTurn,
        phase: Option<Arc<tokio::sync::Mutex<Phase<C>>>>,
        group_phase: Option<Arc<tokio::sync::Mutex<sleep::groups::Phase<C>>>>,
    ) -> Result<String, Aux> {
        let shared = &self.shared;
        let chat = request.chat;
        let slot = shared.turns().slot(chat);
        let guard = slot.gate.clone().lock_owned().await;
        if slot.live().is_some() {
            return Err(Aux::Conflict(String::from(
                "sleep chat already has a live turn",
            )));
        }
        let mut entry = Entry::master(chat, PersonId::system());
        entry.overrides = Overrides {
            model: Some(request.model),
            effort: Some(request.effort),
            thinking: None,
        };
        entry.source = Origin::Sleep;
        entry.run_id = Some(request.run.clone());
        entry.sleep_run_id = Some(request.run);
        let registry = shared.tools().get();
        entry.whitelist = Whitelist::Restricted(
            registry
                .iter()
                .filter(|e| {
                    e.canonical == "sleep_done"
                        || match request.phase {
                            Kind::Memory => e.canonical.starts_with("sleep_memory_"),
                            Kind::Groups => e.canonical.starts_with("sleep_groups_"),
                            _ => false,
                        }
                })
                .map(|e| e.id)
                .collect(),
        );
        let llm = shared.cfg().llm.clone();
        let ctx = shared
            .db()
            .reader()
            .await?
            .run(move |c| ctx::resolve(c, entry, &llm))
            .await?;
        let old = shared
            .db()
            .reader()
            .await?
            .run(move |c| c.turns().open_segment(chat))
            .await?;
        let mut segment = compaction::ensure_segment(shared, &ctx).await?;
        if request.reset
            && let Some(old) = old
        {
            let snap = compaction::snapshot(shared, &ctx).await?;
            let now = shared.now();
            segment = shared
                .db()
                .write(move |tx| {
                    tx.turns().close_segment(old.id, now)?;
                    let new = tx.turns().insert_segment(&NewSegment {
                        chat,
                        reason: SegmentReason::Reset,
                        summary: None,
                        verbatim_tail: None,
                        summary_model: None,
                        summary_path: None,
                        summary_attempts: 0,
                        system_snapshot: snapshot::encode(&snap.system),
                        tools_snapshot: snapshot::encode(&snap.tools),
                        hot_memory_ids: snapshot::encode(&snap.hot_memory_ids),
                        now,
                    })?;
                    log::append(
                        tx,
                        Append {
                            chat,
                            segment: old.id,
                            turn_id: None,
                            step: None,
                            now,
                        },
                        &Payload::SegmentReset(SegmentResetPayload {
                            new_segment: new,
                            reason: SegmentReason::Reset,
                        }),
                    )?;
                    tx.turns().clear_anchor(chat)?;
                    tx.turns().clear_reset_pending(chat)?;
                    DbResult::Ok(new)
                })
                .await?;
        }
        let turn: eva_protocol::types::TurnId = shared.make_world().mint();
        let id = turn.clone();
        let now = shared.now();
        let event = shared
            .db()
            .write(move |tx| {
                let at = Append {
                    chat,
                    segment,
                    turn_id: Some(&id),
                    step: None,
                    now,
                };
                let event = log::append(
                    tx,
                    at,
                    &Payload::Message(MessagePayload::kernel(vec![Block::text(request.prompt)])),
                )?;
                tx.turns().insert_message(&NewMessage {
                    chat,
                    event,
                    role: Role::User,
                    sender: Some(&PersonId::system()),
                    external_id: None,
                    reply_to_external_id: None,
                    turn_id: Some(&id),
                })?;
                DbResult::Ok(event)
            })
            .await?;
        slot.feed.note(event);
        let live = shared.turns().claim(&slot, turn);
        let mut task = Turn::new(shared.clone(), slot.clone(), live.clone(), ctx, segment);
        task.sleep = phase;
        task.group_phase = group_phase;
        task.max_steps = Some(request.max_steps);
        let report = task.sleep_report.clone();
        let finished = tokio_util::sync::CancellationToken::new();
        let signal = finished.clone();
        shared.turns().spawn(async move {
            task.run().await;
            signal.cancel();
        });
        drop(guard);
        tokio::select! {
            () = finished.cancelled() => {},
            () = request.cancel.cancelled() => { live.cancel(EndedBy::Shutdown); finished.cancelled().await; }
        }
        let report = report.lock().await.clone();
        report.ok_or_else(|| {
            Aux::Unavailable(String::from(
                "sleep phase ended without sleep_done: [no report]",
            ))
        })
    }
}
