//! Динамический контекст шага: время меняется только вместе с содержанием reminder.

use crate::{
    components::Components,
    db::{Db as _, DbResult, Queries as _},
    subsystems::turns::{
        engine::{Flow, Turn},
        log::{Payload, SpeakerKind},
        marker, plan,
    },
};
use eva_protocol::types::turn::{ActorKind, Audience};

pub async fn next<C: Components>(turn: &mut Turn<C>, steps_left: u32) -> Flow<Option<String>> {
    let shared = &turn.shared;
    let ctx = &turn.ctx;
    let mut lines = vec![
        marker::reminder_actor(
            match ctx.actor.kind {
                ActorKind::Master => SpeakerKind::Master,
                ActorKind::Whitelisted => SpeakerKind::User,
                _ => SpeakerKind::Unknown,
            },
            ctx.actor.person_id.as_ref().map(|p| p.as_str()),
            ctx.actor.name.as_deref(),
        ),
        marker::reminder_trust(ctx.trusted_chat, ctx.audience == Audience::MasterOnly),
    ];
    if !ctx.master_turn {
        lines.push(crate::subsystems::memory::tools::NON_MASTER_NOTE.to_owned());
    }
    let chat = ctx.chat;
    let loaded = turn.load().await?;
    let first = loaded
        .events
        .iter()
        .find(|e| {
            e.turn_id.as_ref() == Some(&turn.live.turn)
                && matches!(&e.payload, Payload::Message(m) if m.speaker.is_some())
        })
        .map(|e| e.id);
    let (rows, previous) = shared
        .db()
        .reader()
        .await?
        .run(move |c| {
            DbResult::Ok((
                c.agents().plan(chat)?,
                first
                    .map(|id| c.turns().last_human_before(chat, id))
                    .transpose()?
                    .flatten(),
            ))
        })
        .await?;
    if let Some(previous) = previous {
        lines.push(marker::reminder_gap(std::time::Duration::from_nanos(
            turn.started_at
                .as_nanos()
                .get()
                .saturating_sub(previous.as_nanos().get()),
        )));
    }
    if let Some(role) = shared.ports().work.role_line(shared, chat).await {
        lines.push(role);
    }
    lines.extend(
        shared
            .ports()
            .work
            .inbox_lines(shared, chat, shared.cfg().work.inbox_budget)
            .await,
    );
    if !rows.is_empty() {
        lines.push(plan::render(&rows));
    }
    if let Some(pressure) = shared
        .ports()
        .notes
        .pressure_line(shared, chat, turn.model_id())
        .await
    {
        lines.push(pressure);
    }
    if turn
        .reminded
        .as_ref()
        .is_some_and(|(segment, _)| *segment != turn.segment)
        && let Some(nudge) = shared.ports().notes.after_compaction(shared, chat).await
    {
        lines.push(nudge);
    }
    if steps_left < shared.cfg().work.wrapup_steps {
        lines.push(marker::wrapup(steps_left));
    }
    let dynamic = lines.join("\n");
    if turn
        .reminded
        .as_ref()
        .is_some_and(|(segment, prev)| *segment == turn.segment && *prev == dynamic)
        && !turn.plan_changed
    {
        return Ok(None);
    }
    turn.reminded = Some((turn.segment, dynamic.clone()));
    turn.plan_changed = false;
    Ok(Some(format!(
        "{}\n{}\n{}",
        marker::REMINDER,
        marker::reminder_now(shared.now(), &shared.day_zone().tz()),
        dynamic
    )))
}
