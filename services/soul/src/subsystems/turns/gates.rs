//! Вход шага в решётку Exposure: предикаты `needs` (`02 §3.1`, `07 §1`) и
//! внимание чата на сутки (`02 §5`). Считаются перед каждым шагом (I3).

use std::collections::BTreeSet;

use eva_protocol::{
    errors::Aux,
    types::{ModelId, tool::Need},
};

use crate::{
    components::Components,
    db::{Db as _, DbResult, Queries as _},
    shared::Shared,
    subsystems::{
        tools::{
            expose::{DayState, Gates},
            step::{self, StepTools},
        },
        turns::ctx::Ctx,
    },
};

/// Предикаты чата, которые не дёшево держать в `Ctx`.
struct Facts {
    workdir_on: Option<String>,
    workdir: bool,
    agent: bool,
    team: bool,
    plan: bool,
}

pub async fn gates<C: Components>(shared: &Shared<C>, ctx: &Ctx) -> Result<Gates, Aux> {
    let chat = ctx.chat;
    let facts = shared
        .db()
        .reader()
        .await?
        .run(move |c| {
            let row = c.chats().get(chat)?;
            let team = c.agents().live_children(chat)? > 0;
            let plan = !c.agents().plan(chat)?.is_empty();
            DbResult::Ok(row.map(|row| Facts {
                workdir_on: row.workdir_on,
                workdir: row.workdir.is_some(),
                agent: row.agent,
                team,
                plan,
            }))
        })
        .await?
        .ok_or_else(|| Aux::NotFound(format!("chat {chat} does not exist")))?;

    let cfg = shared.cfg();
    let endpoint = facts
        .workdir_on
        .clone()
        .or_else(|| cfg.hands.default_endpoint.clone());
    let index_notes = shared.ports().notes.index_mode(shared, chat).await;
    let embedder = cfg.memory.embedder.as_deref().is_some_and(|name| {
        shared
            .models()
            .get(&ModelId::from(name))
            .is_some_and(|m| m.decl.embed.is_some())
    });
    let mut available = BTreeSet::new();
    let mut put = |need: Need, on: bool| {
        if on {
            available.insert(need);
        }
    };
    put(
        Need::Hands,
        endpoint.is_some_and(|e| shared.hands().get(&e).is_some()),
    );
    put(Need::Lead, !facts.agent);
    put(Need::Agent, facts.agent);
    put(Need::Team, facts.team);
    put(
        Need::Work,
        facts.plan || facts.workdir || facts.agent || facts.team,
    );
    put(Need::Private, !ctx.public());
    put(Need::AskUi, shared.live().ask_ui(&ctx.surface));
    put(Need::IndexNotes, index_notes);
    put(Need::Sleep, ctx.sleep_run_id.is_some());
    put(Need::Embedder, embedder);

    Ok(Gates {
        master_turn: ctx.master_turn,
        trusted_chat: ctx.trusted_chat,
        whitelist: ctx.whitelist.clone(),
        available,
    })
}

pub async fn day_state<C: Components>(shared: &Shared<C>, ctx: &Ctx) -> Result<DayState, Aux> {
    let (chat, now) = (ctx.chat, shared.now());
    let day = shared.day_zone().day(now);
    Ok(shared
        .db()
        .reader()
        .await?
        .run(move |c| {
            let tools = c.tools();
            DbResult::Ok(DayState {
                warm: tools.warm(chat, &day)?,
                pinned: tools.pins(chat, now)?,
                loaded: tools.loaded(chat)?,
                overrides: tools.overrides()?,
            })
        })
        .await?)
}

/// Снимок шага: реестр сейчас, гейты и внимание чата.
pub async fn step_tools<C: Components>(shared: &Shared<C>, ctx: &Ctx) -> Result<StepTools, Aux> {
    let gates = gates(shared, ctx).await?;
    let mut day = day_state(shared, ctx).await?;
    let catalog = shared.tools().get();
    let chat = ctx.chat;
    let names = shared
        .db()
        .reader()
        .await?
        .run(move |c| {
            let Some(segment) = c.turns().open_segment(chat)? else {
                return Ok(Vec::new());
            };
            let loaded = super::log::load(c, segment.id, None)?;
            let Some(loaded) = loaded else {
                return Ok(Vec::new());
            };
            let mut names = loaded
                .snapshot
                .tools
                .into_iter()
                .filter(|s| !s.deferred)
                .map(|s| s.name)
                .collect::<Vec<_>>();
            for event in loaded.events {
                if let super::log::Payload::ToolsAdded(added) = event.payload {
                    names.extend(added.tools.into_iter().map(|s| s.name));
                }
            }
            Result::<_, Aux>::Ok(names)
        })
        .await?;
    if let Some(warm) = &mut day.warm {
        warm.extend(
            names
                .iter()
                .filter_map(|name| catalog.by_canonical(name).map(|entry| entry.id)),
        );
    }
    Ok(step::snapshot(
        &catalog,
        &gates,
        &day,
        &shared.cfg().tools.core,
        &ctx.extra_tools,
    ))
}

/// Снимок нового сегмента применяет отложенные удаления из warm-набора.
pub async fn fresh_step_tools<C: Components>(
    shared: &Shared<C>,
    ctx: &Ctx,
) -> Result<StepTools, Aux> {
    let gates = gates(shared, ctx).await?;
    let day = day_state(shared, ctx).await?;
    let catalog = shared.tools().get();
    Ok(step::snapshot(
        &catalog,
        &gates,
        &day,
        &shared.cfg().tools.core,
        &ctx.extra_tools,
    ))
}

/// После паузы удаления из warm-набора применяются свежим сегментом.
pub async fn idle_reset<C: Components>(shared: &Shared<C>, ctx: &Ctx) -> Result<bool, Aux> {
    let chat = ctx.chat;
    let (row, segment) = shared
        .db()
        .reader()
        .await?
        .run(move |c| DbResult::Ok((c.chats().get(chat)?, c.turns().open_segment(chat)?)))
        .await?;
    let (Some(row), Some(segment)) = (row, segment) else {
        return Ok(false);
    };
    let elapsed = shared
        .now()
        .as_nanos()
        .get()
        .saturating_sub(row.last_activity_at.as_nanos().get());
    if u128::from(elapsed) < shared.cfg().tools.warm.apply_idle.0.as_nanos() {
        return Ok(false);
    }
    let snapshot = super::snapshot::SegmentSnapshot::from_row(&segment).map_err(Aux::from)?;
    let fresh = fresh_step_tools(shared, ctx).await?;
    let current: BTreeSet<_> = fresh.specs.iter().map(|s| s.canonical.as_str()).collect();
    Ok(snapshot
        .tools
        .iter()
        .any(|tool| !tool.deferred && !current.contains(tool.name.as_str())))
}
