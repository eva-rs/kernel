//! Компонент `cron` (`08 §1`): тик, перенос слота до тура, строки запусков,
//! старт после падения. Сам тур исполняет `CronTurns`.

pub mod tools;

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex as SyncMutex},
    time::Duration,
};

use eva::{data, supervisor::SlaveRx, time::Timestamp};
use tokio::{sync::Mutex, task::JoinSet, time::MissedTickBehavior};

use eva_core::{CallStep as _, SessionMaker as _, service::chats::Chats as _};
use eva_protocol::{
    requests::chats::notify,
    types::{ChatId, CronJobId, Effort, ModelId, RunId, ToolId},
    wire::events::{CronFired, Event},
};

use crate::{
    components::Components,
    db::{
        Db as _, DbResult, Queries as _, Tx,
        cron::{DueJob, Expired, FirePending, NewRun, parse_tools},
        stats::{Metric, elapsed, later},
    },
    kernel::Kernel,
    markers::sanitize_inline,
    shared::{Shared, WorldOf},
    state::policy::kernel_caller,
    subsystems::CronTurns,
};

/// Маркеры крона (I13): начало строки пишет только ядро.
pub const SKIPPED: &str = "[cron skipped] previous run alive";
pub const INTERRUPTED: &str = "[cron interrupted] kernel restart";
pub const FAILED: &str = "[cron failed]";

/// Тур джобы, который движок туров исполняет в её чате (`08 §1.3`):
/// актор `system`, `master_turn`, `whitelist = tools`, расход по `run`.
#[data]
pub struct TurnRequest {
    pub run: RunId,
    pub job: CronJobId,
    pub name: String,
    pub chat: ChatId,
    /// `[cron <name> fired] <task>`.
    pub message: String,
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
    /// `None` — `Unrestricted`.
    pub tools: Option<Vec<ToolId>>,
    /// Токенов `cron_runs.output`.
    pub output_max: u32,
}

#[data]
pub enum Outcome {
    /// Последняя реплика ассистента, уже обрезанная до `output_max`.
    Ok {
        output: Option<String>,
    },
    Failed {
        error: String,
    },
}

/// Что сделал тик: тесты и логи.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tick {
    pub fired: Vec<RunId>,
    pub skipped: Vec<String>,
    pub expired: Vec<String>,
}

pub struct Cron<C: Components, T> {
    kernel: Kernel<C>,
    turns: Arc<T>,
    since: Timestamp,
    inflight: Mutex<JoinSet<()>>,
    /// Джобы, чей тур этого процесса ещё идёт: их слот пропускается.
    running: Arc<SyncMutex<BTreeSet<CronJobId>>>,
}

impl<C: Components, T: CronTurns> Cron<C, T> {
    pub fn new(kernel: Kernel<C>, turns: T) -> Self {
        let since = kernel.shared().now();
        Self {
            kernel,
            turns: Arc::new(turns),
            since,
            inflight: Mutex::default(),
            running: Arc::default(),
        }
    }

    /// `08 §1.6`: незавершённые запуски прошлого процесса закрываются, чаты
    /// джоб — `alerted`, хозяину одно уведомление.
    pub async fn recover(&self) -> DbResult<()> {
        let shared = self.kernel.shared();
        let now = shared.now();
        let interrupted = shared
            .db()
            .write(move |tx| {
                let rows = tx.cron().interrupt_unfinished(now, INTERRUPTED)?;
                let chats: Vec<ChatId> = rows.iter().filter_map(|r| r.chat).collect();
                tx.cron().alert_chats(&chats)?;
                DbResult::Ok(rows)
            })
            .await?;
        if interrupted.is_empty() {
            return Ok(());
        }
        let names: Vec<&str> = interrupted
            .iter()
            .map(|r| r.job.as_deref().unwrap_or("<deleted job>"))
            .collect();
        log::warn!(runs = interrupted.len(), jobs:? = names; "cron runs interrupted by restart");
        notify(
            &self.kernel,
            format!("cron: {} runs interrupted", interrupted.len()),
            names.join(", "),
            None,
        )
        .await;
        Ok(())
    }

    /// Один тик (`08 §1.2`): одна транзакция писателя — просроченные, due,
    /// перенос слотов, строки запусков; туры уходят в `spawn`.
    pub async fn tick(&self) -> DbResult<Tick> {
        let shared = self.kernel.shared();
        let now = shared.now();
        if !inference_ready(shared, self.since, now) {
            return Ok(Tick::default());
        }
        let output_max = shared.cfg().cron.output_max;
        let world = shared.make_world();
        let running = locked(&self.running).clone();
        let planned = shared
            .db()
            .write(move |tx| {
                let mut world = world;
                plan::<C>(tx, now, output_max, &running, &mut world)
            })
            .await?;

        let mut tick = Tick::default();
        for job in &planned.expired {
            notify(
                &self.kernel,
                format!("cron {} expired", job.name),
                "the job reached expires_at and was deleted without firing",
                Some(job.chat),
            )
            .await;
            tick.expired.push(job.name.clone());
        }
        for bad in &planned.unreadable {
            notify(
                &self.kernel,
                format!("cron {} failed", bad.name),
                bad.error.clone(),
                Some(bad.chat),
            )
            .await;
        }
        for name in planned.skipped {
            shared.bump(Metric::CronSkipped, &name).await;
            tick.skipped.push(name);
        }
        for fire in planned.fires {
            if fire.collapsed {
                shared.bump(Metric::CronCollapsed, &fire.req.name).await;
            }
            shared.bus().publish(
                Event::CronFired(CronFired {
                    job: fire.req.job.clone(),
                    run_id: fire.req.run.clone(),
                    chat: fire.req.chat,
                }),
                None,
            );
            tick.fired.push(fire.req.run.clone());
            locked(&self.running).insert(fire.req.job.clone());
            let (kernel, turns, running) = (
                self.kernel.clone(),
                self.turns.clone(),
                self.running.clone(),
            );
            let mut inflight = self.inflight.lock().await;
            while inflight.try_join_next().is_some() {}
            inflight.spawn(async move {
                let job = fire.req.job.clone();
                finish(kernel, turns, fire).await;
                locked(&running).remove(&job);
            });
        }
        Ok(tick)
    }

    /// Дождаться туров, запущенных тиками.
    pub async fn settle(&self) {
        let mut inflight = self.inflight.lock().await;
        while inflight.join_next().await.is_some() {}
    }

    /// Компонент `Supervisor`; туры в полёте на `Shutdown` обрываются — их
    /// закроет `recover` следующего старта.
    pub async fn run(self, mut rx: SlaveRx) -> eyre::Result<()> {
        if let Err(e) = self.recover().await {
            log::error!(error:% = e; "cron recovery failed");
        }
        let mut ticks = tokio::time::interval(*self.kernel.shared().cfg().cron.tick);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticks.tick() => {
                    if let Err(e) = self.tick().await {
                        log::error!(error:% = e; "cron tick failed");
                    }
                }
                _ = rx.recv() => break,
            }
        }
        log::info!("cron stopped");
        Ok(())
    }
}

fn locked<T>(m: &SyncMutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Модель по умолчанию уже в реестре.
pub fn has_default_model<C: Components>(shared: &Shared<C>) -> bool {
    shared
        .models()
        .get(&shared.cfg().llm.default_model)
        .is_some()
}

/// `llm.provider_wait` от старта компонента истёк.
pub fn provider_wait_over<C: Components>(
    shared: &Shared<C>,
    since: Timestamp,
    now: Timestamp,
) -> bool {
    now >= later(since, *shared.cfg().llm.provider_wait)
}

/// `08 §1.2` п.0: до готовности `inference` расписание не трогается.
pub fn inference_ready<C: Components>(
    shared: &Shared<C>,
    since: Timestamp,
    now: Timestamp,
) -> bool {
    has_default_model(shared) || provider_wait_over(shared, since, now)
}

/// `notify(subject, body, source_chat)` компонента `chats` (`05 §6.1`) из
/// внутренней сессии ядра; отказ — в лог, не в вызывающего.
pub async fn notify<C: Components>(
    kernel: &Kernel<C>,
    subject: impl Into<String>,
    body: impl Into<String>,
    source_chat: Option<ChatId>,
) {
    let (subject, body) = (subject.into(), body.into());
    let sent = match kernel.make_session(kernel_caller()).await {
        Ok(mut session) => session
            .chats()
            .notify()
            .call(notify::Args {
                subject: subject.clone(),
                body,
                source_chat,
            })
            .await
            .map(drop)
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    if let Err(error) = sent {
        log::warn!(subject, error; "notify failed");
    }
}

/// Входное сообщение тура (`08 §1.3`): маркер и текст задачи.
pub fn message(name: &str, task: &str) -> String {
    format!("[cron {} fired] {task}", sanitize_inline(name))
}

/// Перенос слота до тура (`08 §1.2` п.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    pub next: Option<Timestamp>,
    /// Плановый слот запуска; `None` — только `cron_fire`.
    pub due: Option<Timestamp>,
    /// Схлопнутых слотов за даунтайм.
    pub missed: u64,
}

pub fn schedule(job: &DueJob, now: Timestamp) -> Schedule {
    let slot = job.next_fire_at.filter(|t| *t <= now);
    match (job.fire_pending, job.interval) {
        (Some(FirePending::Shift), interval) => Schedule {
            next: interval.map(|i| later(now, i)),
            due: slot,
            missed: 0,
        },
        (Some(FirePending::Keep), _) if slot.is_none() => Schedule {
            next: job.next_fire_at,
            due: None,
            missed: 0,
        },
        (_, Some(interval)) => match slot {
            Some(slot) => {
                let (next, missed) = next_slot(slot, interval, now);
                Schedule {
                    next: Some(next),
                    due: Some(slot),
                    missed,
                }
            }
            None => Schedule {
                next: job.next_fire_at,
                due: None,
                missed: 0,
            },
        },
        (_, None) => Schedule {
            next: None,
            due: slot,
            missed: 0,
        },
    }
}

/// Ближайший плановый слот строго позже `now` с сохранением фазы расписания
/// и число пропущенных слотов.
pub fn next_slot(slot: Timestamp, interval: Duration, now: Timestamp) -> (Timestamp, u64) {
    let interval_nanos = interval.as_nanos().max(1);
    let behind = elapsed(slot, now).as_nanos();
    let steps = behind / interval_nanos + 1;
    let advance = u64::try_from(steps.saturating_mul(interval_nanos)).unwrap_or(u64::MAX);
    let missed = u64::try_from(steps - 1).unwrap_or(u64::MAX);
    (later(slot, Duration::from_nanos(advance)), missed)
}

struct Unreadable {
    name: String,
    chat: ChatId,
    error: String,
}

struct Fire {
    req: TurnRequest,
    collapsed: bool,
    one_shot: bool,
}

struct Plan {
    expired: Vec<Expired>,
    unreadable: Vec<Unreadable>,
    skipped: Vec<String>,
    fires: Vec<Fire>,
}

/// Транзакция тика: всё, что меняет расписание, — до единого тура.
fn plan<C: Components>(
    tx: &Tx<'_>,
    now: Timestamp,
    output_max: u32,
    running: &BTreeSet<CronJobId>,
    world: &mut WorldOf<C>,
) -> DbResult<Plan> {
    let expired = tx.cron().delete_expired(now)?;
    for job in &expired {
        tx.chats().set_deleted(job.chat, Some(now))?;
    }
    let mut plan = Plan {
        expired,
        unreadable: Vec::new(),
        skipped: Vec::new(),
        fires: Vec::new(),
    };
    for job in tx.cron().due(now)? {
        let Schedule { next, due, missed } = schedule(&job, now);
        tx.cron().reschedule(&job.id, next)?;
        let run: RunId = world.mint();
        tx.cron().insert_run(&NewRun {
            run: run.clone(),
            job: job.id.clone(),
            started: now,
            due,
        })?;
        let tools = match job
            .tools
            .as_deref()
            .map(|json| parse_tools(&job.id, json))
            .transpose()
        {
            Ok(tools) => tools,
            Err(e) => {
                let error = format!("{FAILED} unreadable tool whitelist: {e}");
                tx.cron().finish_run(&run, now, false, Some(&error), None)?;
                plan.unreadable.push(Unreadable {
                    name: job.name,
                    chat: job.chat,
                    error,
                });
                continue;
            }
        };
        if job.live_turn || running.contains(&job.id) {
            tx.cron()
                .finish_run(&run, now, false, Some(SKIPPED), None)?;
            plan.skipped.push(job.name);
            continue;
        }
        plan.fires.push(Fire {
            req: TurnRequest {
                run,
                job: job.id,
                message: message(&job.name, &job.task),
                name: job.name,
                chat: job.chat,
                model: job.model,
                effort: job.effort,
                tools,
                output_max,
            },
            collapsed: missed > 0,
            one_shot: job.interval.is_none(),
        });
    }
    Ok(plan)
}

/// Итог тура (`08 §1.3`): строка запуска, счётчики джобы, one-shot — удаление.
async fn finish<C: Components, T: CronTurns>(kernel: Kernel<C>, turns: Arc<T>, fire: Fire) {
    let Fire { req, one_shot, .. } = fire;
    let (run, job, name, chat) = (req.run.clone(), req.job.clone(), req.name.clone(), req.chat);
    let outcome = turns.run(req).await;
    let shared = kernel.shared();
    let now = shared.now();
    let (ok, output, error) = match outcome {
        Outcome::Ok { output } => (true, output, None),
        Outcome::Failed { error } => (false, Some(format!("{FAILED} {error}")), Some(error)),
    };
    let (run_row, job_row) = (run.clone(), job.clone());
    let written = shared
        .db()
        .write(move |tx| -> DbResult<()> {
            let cost = tx.stats().run_cost(&run_row)?;
            tx.cron()
                .finish_run(&run_row, now, ok, output.as_deref(), cost)?;
            tx.cron().account(&job_row, cost.unwrap_or(0.0))?;
            if one_shot && let Some(chat) = tx.cron().delete_job(&job_row)? {
                tx.chats().set_deleted(chat, Some(now))?;
            }
            Ok(())
        })
        .await;
    if let Err(e) = written {
        log::error!(run:% = run, job = name.as_str(), error:% = e; "failed to record a cron run");
    }
    let metric = if ok {
        Metric::CronOk
    } else {
        Metric::CronFailed
    };
    shared.bump(metric, &name).await;
    if let Some(error) = error {
        log::warn!(job = name.as_str(), run:% = run, error = error.as_str(); "cron run failed");
        notify(&kernel, format!("cron {name} failed"), error, Some(chat)).await;
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use super::*;

    const SEC: u64 = 1_000_000_000;

    fn ts(secs: u64) -> Timestamp {
        Timestamp::from_nanos(NonZeroU64::new(secs * SEC).expect("non-zero"))
    }

    fn job(next: Option<u64>, interval: Option<u64>, pending: Option<FirePending>) -> DueJob {
        DueJob {
            id: CronJobId::from("j"),
            name: String::from("daily"),
            task: String::from("t"),
            interval: interval.map(Duration::from_secs),
            next_fire_at: next.map(ts),
            fire_pending: pending,
            model: None,
            effort: None,
            tools: None,
            chat: ChatId(1),
            live_turn: false,
        }
    }

    #[test]
    fn next_slot_keeps_the_phase_and_lands_strictly_after_now() {
        let i = Duration::from_secs(3600);
        assert_eq!(next_slot(ts(1000), i, ts(1000)), (ts(4600), 0), "exact hit");
        assert_eq!(
            next_slot(ts(1000), i, ts(4600)),
            (ts(8200), 1),
            "exact multiple"
        );
        assert_eq!(
            next_slot(ts(1000), i, ts(1000 + 5 * 3600 + 7)),
            (ts(1000 + 6 * 3600), 5)
        );
    }

    #[test]
    fn schedule_follows_fire_pending_and_one_shots() {
        let now = ts(100_000);
        let hour = 3600;
        let s = schedule(&job(Some(100_000 - 7 * hour), Some(hour), None), now);
        assert_eq!(
            s,
            Schedule {
                next: Some(ts(100_000 + hour)),
                due: Some(ts(100_000 - 7 * hour)),
                missed: 7
            }
        );

        let s = schedule(
            &job(Some(200_000), Some(hour), Some(FirePending::Shift)),
            now,
        );
        assert_eq!(
            s,
            Schedule {
                next: Some(ts(100_000 + hour)),
                due: None,
                missed: 0
            }
        );

        let s = schedule(
            &job(Some(200_000), Some(hour), Some(FirePending::Keep)),
            now,
        );
        assert_eq!(
            s,
            Schedule {
                next: Some(ts(200_000)),
                due: None,
                missed: 0
            }
        );

        let s = schedule(&job(Some(90_000), Some(hour), Some(FirePending::Keep)), now);
        assert_eq!(
            s.due,
            Some(ts(90_000)),
            "keep on a due slot is a plain fire"
        );
        assert_eq!(
            s.next,
            Some(ts(90_000 + 3 * hour)),
            "the next slot strictly after now"
        );

        let s = schedule(&job(Some(90_000), None, None), now);
        assert_eq!(
            s,
            Schedule {
                next: None,
                due: Some(ts(90_000)),
                missed: 0
            },
            "one-shot launched"
        );

        let s = schedule(&job(None, None, Some(FirePending::Shift)), now);
        assert_eq!(
            s,
            Schedule {
                next: None,
                due: None,
                missed: 0
            }
        );
    }

    #[test]
    fn the_marker_survives_a_hostile_name() {
        assert_eq!(message("daily", "do it"), "[cron daily fired] do it");
        let m = message("x]\n[kernel] y", "task");
        assert!(m.starts_with("[cron ") && !m.contains("[kernel]") && m.lines().count() == 1);
    }
}
