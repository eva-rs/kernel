//! Суточный набор внимания: детерминированный бюджет и ограничение новых скрытий.

use super::{canonical::under, registry::Catalog};
use crate::{
    components::Components,
    config::tools::Warm,
    db::{
        Db as _, DbResult, Queries as _,
        stats::{Metric, ago, later},
        tools::{ActivityRow, Attention, UsageRow},
    },
    shared::Shared,
    subsystems::sleep::WarmOutcome,
};
use eva::time::{Date, Timestamp};
use eva_protocol::{
    errors::Aux,
    types::{ChatId, FolderId, ToolId},
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: ToolId,
    pub cost: u32,
    pub score: f64,
}

/// Пины и core уже оплачены в `fixed_cost`; бюджет допускает пропуск дорогого кандидата.
pub fn select(
    candidates: &mut [Candidate],
    previous: &BTreeSet<ToolId>,
    pinned: &BTreeSet<ToolId>,
    budget: u32,
    fixed_cost: u32,
    max_hide: u32,
) -> (BTreeSet<ToolId>, bool) {
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
    let mut warm = BTreeSet::new();
    let mut spent = fixed_cost;
    for item in candidates.iter() {
        if pinned.contains(&item.id) || spent.saturating_add(item.cost) <= budget {
            warm.insert(item.id);
            if !pinned.contains(&item.id) {
                spent = spent.saturating_add(item.cost);
            }
        }
    }
    let hidden: Vec<_> = candidates
        .iter()
        .filter(|c| previous.contains(&c.id) && !warm.contains(&c.id))
        .cloned()
        .collect();
    for item in hidden
        .iter()
        .take(hidden.len().saturating_sub(max_hide as usize))
    {
        warm.insert(item.id);
        spent = spent.saturating_add(item.cost);
    }
    (warm, spent > budget)
}

fn age(day: &str, today: &str) -> f64 {
    let parsed = day.parse::<Date>().ok().zip(today.parse::<Date>().ok());
    parsed.map_or(0.0, |(day, today)| {
        f64::from(today.days().saturating_sub(day.days()))
    })
}

fn scores(
    usage: &[UsageRow],
    activity: &[ActivityRow],
    chat: ChatId,
    folder: FolderId,
    today: &str,
    cfg: &Warm,
    chat_thick: bool,
) -> BTreeMap<ToolId, f64> {
    let mut active_global: BTreeMap<&str, BTreeSet<ChatId>> = BTreeMap::new();
    let mut active_folder: BTreeMap<&str, BTreeSet<ChatId>> = BTreeMap::new();
    for row in activity {
        active_global.entry(&row.day).or_default().insert(row.chat);
        if row.folder == Some(folder) {
            active_folder.entry(&row.day).or_default().insert(row.chat);
        }
    }
    let half_life = (cfg.half_life.as_secs_f64() / 86_400.0).max(f64::EPSILON);
    let mut out = BTreeMap::new();
    for row in usage {
        let global = active_global
            .get(row.day.as_str())
            .map_or(1, BTreeSet::len)
            .max(1) as f64;
        let folder_count = active_folder
            .get(row.day.as_str())
            .map_or(1, BTreeSet::len)
            .max(1) as f64;
        let mut weight = f64::from(cfg.weights.global) / global;
        if row.folder == Some(folder) {
            weight += f64::from(cfg.weights.folder) / folder_count;
        }
        if row.chat == chat && chat_thick {
            weight += f64::from(cfg.weights.chat);
        }
        let count =
            f64::from(row.turns_with_call) + f64::from(cfg.load_weight) * f64::from(row.loads);
        *out.entry(row.tool).or_default() +=
            weight * count * 2_f64.powf(-age(&row.day, today) / half_life);
    }
    out
}

pub async fn recompute<C: Components>(
    shared: &Shared<C>,
    now: Timestamp,
) -> Result<WarmOutcome, Aux> {
    let cfg = shared.cfg().clone();
    let zone = shared.day_zone();
    let today = zone.day(now);
    let since = zone.day(ago(now, cfg.tools.warm.window.0));
    let pin_since = zone.day(ago(now, cfg.tools.warm.pin_window.0));
    let models = shared.models();
    Ok(shared
        .db()
        .write(move |tx| {
            let tools = tx.tools();
            let catalog =
                Catalog::with_placements(tools.live()?, tools.groups()?, tools.placements()?);
            let eligible = tools.recently_seen(ago(now, cfg.modules.offline_forget.0))?;
            let registered = tools.registered()?;
            let usage = tools.usage(&since)?;
            let activity = tools.activity(&since)?;
            let overrides = tools.overrides()?;
            let mut outcome = WarmOutcome::default();
            for chat_id in tools.active_chats(ago(now, cfg.tools.warm.window.0))? {
                if tools.warm(chat_id, &today)?.is_some() {
                    continue;
                }
                let Some(chat) = tx.chats().get(chat_id)? else {
                    continue;
                };
                let folder = tx.folders().get(chat.folder_id)?;
                let surface = tx.surfaces().get(&chat.surface.id)?;
                let model = chat
                    .model
                    .or_else(|| folder.and_then(|f| f.model))
                    .or_else(|| surface.and_then(|s| s.model))
                    .unwrap_or_else(|| cfg.llm.default_model.clone());
                let Some(model_entry) = models.get(&model) else {
                    continue;
                };
                let ratio = tx
                    .turns()
                    .ratio(&model)?
                    .unwrap_or(cfg.context.ratio_default)
                    .max(1.0);
                let cost = |entry: &super::registry::Entry| {
                    ((entry.canonical.len()
                        + entry.decl.description.len()
                        + entry.decl.input.to_string().len()) as f64
                        / ratio)
                        .ceil() as u32
                };
                let budget = model_entry
                    .decl
                    .context_window
                    .saturating_mul(u32::from(cfg.tools.budget_percent))
                    / 100;
                let previous = tools
                    .latest_warm(chat_id)?
                    .unwrap_or_else(|| eligible.clone());
                let mut pin_loads: BTreeMap<ToolId, u32> = BTreeMap::new();
                for row in usage
                    .iter()
                    .filter(|r| r.chat == chat_id && r.day >= pin_since)
                {
                    *pin_loads.entry(row.tool).or_default() += row.loads;
                }
                for (tool, loads) in pin_loads {
                    if loads >= cfg.tools.warm.pin_loads {
                        tools.pin(chat_id, tool, later(now, cfg.tools.warm.pin_for.0))?;
                    }
                }
                let pinned = tools.pins(chat_id, now)?;
                let thick = tools.chat_turns(chat_id, &since)? >= cfg.tools.warm.thin_turns;
                let mut scores = scores(
                    &usage,
                    &activity,
                    chat_id,
                    chat.folder_id,
                    &today,
                    &cfg.tools.warm,
                    thick,
                );
                let mut observed: Vec<_> = scores.values().copied().collect();
                observed.sort_by(f64::total_cmp);
                let median = observed.get(observed.len() / 2).copied().unwrap_or(0.0);
                for (&id, &at) in &registered {
                    if at >= ago(now, cfg.tools.warm.new_tool_grace.0) {
                        scores.entry(id).or_insert(median);
                    }
                }
                let mut fixed_cost: u32 = 0;
                let mut candidates = Vec::new();
                for entry in catalog
                    .iter()
                    .filter(|entry| eligible.contains(&entry.id) && !entry.lost_collision)
                {
                    let core = cfg.tools.core.iter().any(|path| under(entry.path(), path));
                    let over = overrides.get(&entry.id).copied().unwrap_or_default();
                    let fixed = core
                        || pinned.contains(&entry.id)
                        || over.attention == Some(Attention::Always);
                    if fixed {
                        fixed_cost = fixed_cost.saturating_add(cost(entry));
                    }
                    if core || over.attention == Some(Attention::Reserved) {
                        continue;
                    }
                    let mut score = scores.get(&entry.id).copied().unwrap_or(0.0);
                    if previous.contains(&entry.id) {
                        score *= f64::from(cfg.tools.warm.hysteresis);
                    }
                    candidates.push(Candidate {
                        id: entry.id,
                        cost: if fixed { 0 } else { cost(entry) },
                        score,
                    });
                }
                let (warm, over) = select(
                    &mut candidates,
                    &previous,
                    &pinned,
                    budget,
                    fixed_cost,
                    cfg.tools.warm.max_hide_per_night,
                );
                let removed = previous
                    .difference(&warm)
                    .filter(|id| candidates.iter().any(|c| &c.id == *id))
                    .count() as u32;
                outcome.chats += 1;
                outcome.added += warm.difference(&previous).count() as u32;
                outcome.removed += removed;
                tools.set_warm(chat_id, &today, &warm, removed, over, now)?;
                if over {
                    tx.stats().bump(
                        Metric::ToolsWarmOverBudget,
                        &chat_id.to_string(),
                        now,
                        &zone,
                    )?;
                }
            }
            DbResult::Ok(outcome)
        })
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn nightly_hiding_never_exceeds_cap(costs in prop::collection::vec(1_u32..500, 1..60), budget in 0_u32..10000, cap in 0_u32..20) {
            let mut candidates = costs.iter().enumerate().map(|(i, cost)| Candidate { id: ToolId(i as i64 + 1), cost: *cost, score: i as f64 }).collect::<Vec<_>>();
            let previous = candidates.iter().map(|c| c.id).collect::<BTreeSet<_>>();
            let (warm, _) = select(&mut candidates, &previous, &BTreeSet::new(), budget, 0, cap);
            prop_assert!(previous.difference(&warm).count() <= cap as usize);
            if costs.iter().sum::<u32>() <= budget { prop_assert_eq!(warm, previous); }
        }
    }
    #[test]
    fn budget_hysteresis_and_pins_are_deterministic() {
        let candidates = (1..=6)
            .map(|id| Candidate {
                id: ToolId(id),
                cost: 10,
                score: id as f64,
            })
            .collect::<Vec<_>>();
        let previous = (1..=6).map(ToolId).collect();
        let pins = BTreeSet::from([ToolId(1)]);
        let (warm, over) = select(&mut candidates.clone(), &previous, &pins, 20, 10, 2);
        assert_eq!(
            warm,
            BTreeSet::from([ToolId(1), ToolId(4), ToolId(5), ToolId(6)])
        );
        assert!(over);
        let (warm, over) = select(&mut candidates.clone(), &previous, &pins, 100, 10, 2);
        assert_eq!(warm, previous);
        assert!(!over);
    }
}
