//! После рестарта каждый незавершённый вызов и тур получают устойчивый исход.

use crate::{
    components::Components,
    db::{Db as _, DbResult, Queries as _, stats::Metric},
    shared::Shared,
    subsystems::turns::{
        dispatch,
        log::{self, Append, Payload, Synthetic, TurnEndedPayload},
    },
};
use eva_protocol::{errors::Aux, types::turn::TurnOutcome};
use std::collections::BTreeSet;

pub async fn recover<C: Components>(shared: &Shared<C>) -> Result<(), Aux> {
    let now = shared.now();
    let count = shared
        .db()
        .write(move |tx| {
            let turns = tx.turns().open_turns()?;
            for turn in &turns {
                let rows = tx
                    .turns()
                    .turn_events_after(&turn.turn, eva_protocol::types::EventId(0))?;
                let events = log::decode_all(rows).map_err(crate::db::DbError::corrupt)?;
                let closed: BTreeSet<&str> = events
                    .iter()
                    .filter_map(|e| match &e.payload {
                        Payload::ToolResult(r) => Some(r.call_id.as_str()),
                        _ => None,
                    })
                    .collect();
                let at = Append {
                    chat: turn.chat,
                    segment: turn.segment,
                    turn_id: Some(&turn.turn),
                    step: None,
                    now,
                };
                for event in &events {
                    if let Payload::ToolUse(call) = &event.payload
                        && !closed.contains(call.call_id.as_str())
                    {
                        dispatch::write_synthetic(
                            tx,
                            at,
                            &call.call_id,
                            call.tool_id,
                            Synthetic::Interrupted,
                        )?;
                    }
                }
                log::append(
                    tx,
                    at,
                    &Payload::TurnEnded(TurnEndedPayload {
                        turn_id: turn.turn.clone(),
                        outcome: TurnOutcome::Interrupted,
                        by: None,
                        error: Some(String::from("interrupted by restart")),
                    }),
                )?;
            }
            tx.agents().cancel_pending(None, now)?;
            tx.tools().all_offline()?;
            DbResult::Ok(turns.len())
        })
        .await?;
    for _ in 0..count {
        shared.bump(Metric::TurnsInterrupted, "restart").await;
    }
    Ok(())
}
