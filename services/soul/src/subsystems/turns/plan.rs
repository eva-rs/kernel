//! План тура (`07 §5`): операция считается на копии, `normalize` держит
//! число активных задач, запись — DELETE+INSERT; отказ следов не оставляет.
//! Рендер `[plan]` один для ответа тула и reminder.

use eva::data;

use eva_protocol::{
    errors::Aux,
    types::{
        ChatId,
        turn::{FeedEvent, ItemDelta, PlanTask, TaskStatus},
    },
    wire::tools::ToolResult,
};

use crate::{
    components::Components,
    db::{Db as _, DbResult, Queries as _, agents::PlanRow},
    shared::Shared,
    subsystems::turns::{engine::Turn, estimate::Estimator, feed, marker},
};

/// One plan operation.
#[data]
pub struct Update {
    pub op: Op,
}

#[data]
pub enum Op {
    /// Replace the plan with these tasks.
    Init { tasks: Vec<NewTask> },
    /// Make a task active.
    Start { id: u32 },
    /// Close a task (the active one by default).
    Done {
        id: Option<u32>,
        outcome: Option<String>,
    },
    /// Abandon a task (the active one by default) with a reason.
    Drop { id: Option<u32>, outcome: String },
    /// Add tasks at the end of their phase.
    Append { tasks: Vec<NewTask> },
    /// Show the plan.
    View,
}

#[data]
pub struct NewTask {
    pub title: String,
    pub phase: Option<String>,
}

/// Отказ операции: текст `is_error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected(pub String);

const fn open(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Pending | TaskStatus::InProgress)
}

/// Операция на копии; `now` — `updated_at` тронутых строк.
pub fn apply(
    rows: &[PlanRow],
    op: &Op,
    now: eva::time::Timestamp,
    live_agents: u32,
) -> Result<Vec<PlanRow>, Rejected> {
    let mut plan = rows.to_vec();
    let next_id = |plan: &[PlanRow]| plan.iter().map(|r| r.task_id).max().unwrap_or(0) + 1;
    let find = |plan: &[PlanRow], id: Option<u32>| -> Result<usize, Rejected> {
        match id {
            Some(id) => plan
                .iter()
                .position(|r| r.task_id == id)
                .ok_or_else(|| Rejected(format!("unknown task {id}"))),
            None => plan
                .iter()
                .position(|r| r.status == TaskStatus::InProgress)
                .ok_or_else(|| Rejected(String::from("no active task"))),
        }
    };
    let new = |t: &NewTask, id: u32, position: u32| PlanRow {
        task_id: id,
        position,
        phase: t.phase.clone().filter(|p| !p.trim().is_empty()),
        title: t.title.clone(),
        status: TaskStatus::Pending,
        outcome: None,
        updated_at: now,
    };
    let mut keep: Option<u32> = None;
    match op {
        Op::View => return Ok(plan),
        Op::Init { tasks } => {
            if tasks.is_empty() {
                return Err(Rejected(String::from("init needs at least one task")));
            }
            let base = next_id(&plan);
            plan = tasks
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let i = u32::try_from(i).unwrap_or(u32::MAX);
                    new(t, base + i, i)
                })
                .collect();
        }
        Op::Append { tasks } => {
            for t in tasks {
                let id = next_id(&plan);
                let phase = t.phase.as_deref().map(str::to_lowercase);
                let at = plan
                    .iter()
                    .rposition(|r| r.phase.as_deref().map(str::to_lowercase) == phase)
                    .map_or(plan.len(), |i| i + 1);
                plan.insert(at, new(t, id, 0));
            }
        }
        Op::Start { id } => {
            let i = find(&plan, Some(*id))?;
            if !open(plan[i].status) {
                return Err(Rejected(format!("task {id} is closed")));
            }
            plan[i].status = TaskStatus::InProgress;
            plan[i].updated_at = now;
            keep = Some(*id);
        }
        Op::Done { id, outcome } => {
            let i = find(&plan, *id)?;
            if !open(plan[i].status) {
                return Err(Rejected(String::from("task is already closed")));
            }
            plan[i].status = TaskStatus::Completed;
            plan[i].outcome = outcome.clone();
            plan[i].updated_at = now;
        }
        Op::Drop { id, outcome } => {
            let i = find(&plan, *id)?;
            if !open(plan[i].status) {
                return Err(Rejected(String::from("task is already closed")));
            }
            plan[i].status = TaskStatus::Abandoned;
            plan[i].outcome = Some(outcome.clone());
            plan[i].updated_at = now;
        }
    }
    for (i, row) in plan.iter_mut().enumerate() {
        row.position = u32::try_from(i).unwrap_or(u32::MAX);
    }
    normalize(&mut plan, live_agents, keep, now);
    Ok(plan)
}

/// Активных при открытых задачах — от одной до `1 + live_agents`: лишние
/// (последние по позиции, кроме только что начатой) → `pending`, нет
/// активной при `pending` → первая `pending`.
pub fn normalize(
    plan: &mut [PlanRow],
    live_agents: u32,
    keep: Option<u32>,
    now: eva::time::Timestamp,
) {
    let bound = 1 + live_agents as usize;
    let mut active: Vec<usize> = (0..plan.len())
        .filter(|&i| plan[i].status == TaskStatus::InProgress)
        .collect();
    while active.len() > bound {
        let victim = active
            .iter()
            .rposition(|&i| Some(plan[i].task_id) != keep)
            .unwrap_or(active.len() - 1);
        let i = active.remove(victim);
        plan[i].status = TaskStatus::Pending;
        plan[i].updated_at = now;
    }
    if active.is_empty()
        && let Some(first) = plan.iter_mut().find(|r| r.status == TaskStatus::Pending)
    {
        first.status = TaskStatus::InProgress;
        first.updated_at = now;
    }
}

/// Блок `[plan]` (`07 §5` п.7); пустой план — одна строка.
pub fn render(rows: &[PlanRow]) -> String {
    if rows.is_empty() {
        return String::from(marker::PLAN_EMPTY);
    }
    let mut out = String::from(marker::PLAN);
    let mut phase: Option<&str> = None;
    for row in rows {
        if !match (row.phase.as_deref(), phase) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            (None, None) => true,
            _ => false,
        } {
            phase = row.phase.as_deref();
            if let Some(p) = phase {
                out.push_str(&format!("\n## {p}"));
            }
        }
        let status = match row.status {
            TaskStatus::Pending => "pending",
            TaskStatus::InProgress => "active",
            TaskStatus::Completed => "done",
            TaskStatus::Abandoned => "dropped",
            TaskStatus::Unknown => "unknown",
        };
        out.push_str(&format!("\n- {} [{status}] {}", row.task_id, row.title));
        if let Some(outcome) = &row.outcome {
            out.push_str(" — ");
            out.push_str(outcome);
        }
    }
    out
}

pub fn tasks(rows: &[PlanRow]) -> Vec<PlanTask> {
    rows.iter()
        .map(|r| PlanTask {
            id: r.task_id,
            phase: r.phase.clone(),
            title: r.title.clone(),
            status: r.status,
            outcome: r.outcome.clone(),
        })
        .collect()
}

pub async fn load<C: Components>(shared: &Shared<C>, chat: ChatId) -> Result<Vec<PlanRow>, Aux> {
    Ok(shared
        .db()
        .reader()
        .await?
        .run(move |c| c.agents().plan(chat))
        .await?)
}

/// `plan_update`: `true` — план изменился, reminder обновится.
pub async fn update<C: Components>(
    turn: &Turn<C>,
    args: Update,
) -> Result<(ToolResult, bool), Aux> {
    let shared = &turn.shared;
    let chat = turn.ctx.chat;
    let rows = load(shared, chat).await?;
    let live_agents = shared.ports().work.live_subagents(shared, chat).await;
    let next = match apply(&rows, &args.op, shared.now(), live_agents) {
        Ok(next) => next,
        Err(Rejected(why)) => return Ok((ToolResult::error(why), false)),
    };
    let work = &shared.cfg().work;
    if next.len() > work.plan_max_tasks as usize {
        return Ok((
            ToolResult::error(format!(
                "plan exceeds work.plan_max_tasks = {}",
                work.plan_max_tasks
            )),
            false,
        ));
    }
    let text = render(&next);
    let model = turn.model_id();
    let ratio = shared
        .db()
        .reader()
        .await?
        .run(move |c| c.turns().ratio(&model))
        .await?;
    if Estimator::new(ratio, &shared.cfg().context).text(text.len()) > work.plan_budget {
        return Ok((
            ToolResult::error(format!(
                "plan exceeds work.plan_budget = {} tokens",
                work.plan_budget
            )),
            false,
        ));
    }
    let changed = next != rows;
    if changed {
        let write = next.clone();
        shared
            .db()
            .write(move |tx| tx.agents().replace_plan(chat, &write))
            .await?;
        turn.slot.feed.live(FeedEvent::ItemDelta {
            id: feed::plan_id(chat),
            delta: ItemDelta::Plan {
                tasks: tasks(&next),
            },
        });
    }
    Ok((
        ToolResult::ok(vec![eva_protocol::types::tool::ToolContent::text(text)]),
        changed,
    ))
}

pub async fn purge_settled<C: Components>(shared: &Shared<C>, chat: ChatId) -> Result<(), Aux> {
    shared
        .db()
        .write(move |tx| tx.agents().purge_settled(chat).map(drop))
        .await?;
    Ok(())
}

pub async fn delete_if_closed<C: Components>(shared: &Shared<C>, chat: ChatId) -> DbResult<bool> {
    shared
        .db()
        .write(move |tx| tx.agents().delete_plan_if_closed(chat))
        .await
}

#[cfg(test)]
mod tests {
    use eva::time::Timestamp;
    use proptest::prelude::*;

    use super::*;

    fn now() -> Timestamp {
        Timestamp::from_secs(1_750_000_000)
    }

    fn op() -> impl Strategy<Value = Op> {
        let task = (
            "[a-z]{1,8}",
            prop::option::of(prop::sample::select(vec!["a", "B", "b"])),
        )
            .prop_map(|(title, phase)| NewTask {
                title,
                phase: phase.map(String::from),
            });
        prop_oneof![
            prop::collection::vec(task.clone(), 1..4).prop_map(|tasks| Op::Init { tasks }),
            (1_u32..8).prop_map(|id| Op::Start { id }),
            prop::option::of(1_u32..8).prop_map(|id| Op::Done { id, outcome: None }),
            prop::option::of(1_u32..8).prop_map(|id| Op::Drop {
                id,
                outcome: String::from("no"),
            }),
            prop::collection::vec(task, 1..3).prop_map(|tasks| Op::Append { tasks }),
            Just(Op::View),
        ]
    }

    proptest! {
        #[test]
        fn active_bounds_property(ops in prop::collection::vec(op(), 1..20), agents in 0_u32..3) {
            let mut plan: Vec<PlanRow> = Vec::new();
            for op in &ops {
                if let Ok(next) = apply(&plan, op, now(), agents) {
                    plan = next;
                }
                let open = plan.iter().filter(|r| super::open(r.status)).count();
                let active = plan.iter().filter(|r| r.status == TaskStatus::InProgress).count();
                if open > 0 {
                    prop_assert!(active >= 1, "{plan:?}");
                    prop_assert!(active <= 1 + agents as usize, "{plan:?}");
                } else {
                    prop_assert_eq!(active, 0);
                }
                if agents == 0 && open > 0 {
                    prop_assert_eq!(active, 1);
                }
            }
        }
    }

    #[test]
    fn rejection_leaves_rows_untouched_and_append_lands_in_its_phase() {
        let plan = apply(
            &[],
            &Op::Init {
                tasks: vec![
                    NewTask {
                        title: "a".into(),
                        phase: Some("Build".into()),
                    },
                    NewTask {
                        title: "b".into(),
                        phase: Some("Ship".into()),
                    },
                ],
            },
            now(),
            0,
        )
        .expect("init");
        assert_eq!(plan[0].status, TaskStatus::InProgress);
        assert!(apply(&plan, &Op::Start { id: 99 }, now(), 0).is_err());
        let appended = apply(
            &plan,
            &Op::Append {
                tasks: vec![NewTask {
                    title: "c".into(),
                    phase: Some("build".into()),
                }],
            },
            now(),
            0,
        )
        .expect("append");
        let titles: Vec<&str> = appended.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, ["a", "c", "b"]);
        let started = apply(&appended, &Op::Start { id: 2 }, now(), 0).expect("start");
        let active: Vec<u32> = started
            .iter()
            .filter(|r| r.status == TaskStatus::InProgress)
            .map(|r| r.task_id)
            .collect();
        assert_eq!(active, [2], "the started task wins the bound");
        insta::assert_snapshot!("plan_block", render(&started));
        assert_eq!(render(&[]), marker::PLAN_EMPTY);
    }
}
