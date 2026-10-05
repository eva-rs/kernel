//! Компонент `sleep` (`08 §2`): расписание ночи, строки `sleep_runs` /
//! `sleep_phase_runs`, отчёт и уведомление, старт после падения. Туры фаз
//! 1–2 исполняет `SleepTurns`, warm-набор — `WarmSets`.

use std::sync::Arc;

use eva::{data, supervisor::SlaveRx, time::Timestamp};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use eva_protocol::{
    types::{ChatId, Effort, ModelId, RunId, SurfaceId, chat::Kind, sleep::Phase},
    wire::events::{Event, PhaseState, SleepPhase},
};

use crate::{
    components::Components,
    db::{
        Db as _, DbError, DbResult, Queries as _,
        chats::NewChat,
        meta::{SLEEP_CHAT_ID, SLEEP_SKIPPED_AT},
        sleep::PhaseEnd as PhaseRowEnd,
        stats::{Metric, ago, later},
        surfaces,
    },
    kernel::Kernel,
    subsystems::{
        SleepTurns, WarmSets,
        cron::{has_default_model, notify, provider_wait_over},
    },
};

pub mod groups;
pub mod memory;

/// Тур фазы в чате `sleep` (`08 §2.2`): актор `system`, `master_turn`,
/// `whitelist = группа sleep`, расход по `run`.
#[derive(Debug, Clone)]
pub struct PhaseTurn {
    pub run: RunId,
    pub phase: Phase,
    pub chat: ChatId,
    /// Первый тур ночи открывается голым `SegmentReset`: прошлые ночи в промпт не едут.
    pub reset: bool,
    pub model: ModelId,
    pub effort: Effort,
    /// User-сообщение фазы.
    pub prompt: String,
    /// Шагов до отмены тура с `report = "[no report]"`.
    pub max_steps: u32,
    /// `Shutdown`: тур отменяется штатно, применённое остаётся.
    pub cancel: CancellationToken,
}

#[data]
pub enum Outcome {
    /// `report` из `sleep_done`.
    Done {
        report: String,
    },
    Failed {
        error: String,
    },
}

#[data(copy)]
#[derive(Default)]
pub struct WarmOutcome {
    pub chats: u32,
    pub added: u32,
    pub removed: u32,
}

/// Что решил тик (`08 §2.1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tick {
    /// Не due, ждёт тишины или провайдера.
    Idle,
    /// Ночь пропущена: `sleep.enabled = false` или провайдер не пришёл.
    Skipped,
    Slept(RunId),
}

pub struct Sleep<C: Components, E> {
    kernel: Kernel<C>,
    engine: Arc<E>,
    since: Timestamp,
}

/// Строки отчёта и счётчики ночи.
#[derive(Default)]
struct Night {
    lines: Vec<String>,
    applied: u32,
    discarded: u32,
    forgets: u32,
    failed: bool,
    interrupted: bool,
}

impl Night {
    fn phase(&mut self, phase: Phase, outcome: &Outcome, summary: &memory::Summary) -> String {
        let report = match outcome {
            Outcome::Done { report } => report.clone(),
            Outcome::Failed { error } => {
                self.failed = true;
                format!("[error] {error}")
            }
        };
        self.lines.push(format!("{}: {report}", phase_name(phase)));
        self.applied += summary.applied;
        self.discarded += summary.discarded;
        self.forgets += summary.forgets;
        report
    }

    fn report(&self) -> String {
        let mut out = self.lines.join("\n");
        if self.interrupted {
            out.push_str("\n[interrupted]");
        }
        out.push_str(&format!(
            "\napplied {}, discarded {}, forgotten {}",
            self.applied, self.discarded, self.forgets
        ));
        out
    }
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Memory => "memory",
        Phase::Groups => "groups",
        Phase::Warm => "warm",
        Phase::Unknown => "unknown",
    }
}

impl<C: Components, E: SleepTurns<C> + WarmSets> Sleep<C, E> {
    pub fn new(kernel: Kernel<C>, engine: E) -> Self {
        let since = kernel.shared().now();
        Self {
            kernel,
            engine: Arc::new(engine),
            since,
        }
    }

    /// `08 §2.6`: ночи, оборванные падением, закрываются как `interrupted`.
    pub async fn recover(&self) -> DbResult<()> {
        let shared = self.kernel.shared();
        let now = shared.now();
        let runs = shared.db().write(move |tx| tx.sleep().recover(now)).await?;
        for run in &runs {
            log::warn!(run:% = run; "sleep night interrupted by restart");
            shared.bump(Metric::SleepInterrupted, "").await;
        }
        Ok(())
    }

    /// Один тик (`08 §2.1`): due по границе, тишина или `force_after`, ночь.
    pub async fn tick(&self, cancel: &CancellationToken) -> DbResult<Tick> {
        let shared = self.kernel.shared();
        let cfg = &shared.cfg().sleep;
        let now = shared.now();
        let due_at = shared.day_zone().latest_at(cfg.at.0, now);
        let reader = shared.db().reader().await?;
        let last = reader
            .run(|c| {
                let started = c.sleep().last_started()?;
                let skipped = c.meta().timestamp(SLEEP_SKIPPED_AT)?;
                DbResult::Ok(started.max(skipped))
            })
            .await?;
        if last.is_some_and(|last| last >= due_at) {
            return Ok(Tick::Idle);
        }
        if !cfg.enabled {
            self.skip(due_at, "sleep.enabled = false").await?;
            return Ok(Tick::Skipped);
        }
        if !has_default_model(shared) {
            if provider_wait_over(shared, self.since, now) {
                self.skip(due_at, "no provider for the default model")
                    .await?;
                return Ok(Tick::Skipped);
            }
            return Ok(Tick::Idle);
        }
        if now < later(due_at, *cfg.force_after) {
            let quiet = reader.run(|c| c.sleep().quiet()).await?;
            let recent_human = quiet
                .last_human_at
                .is_some_and(|at| at > ago(now, *cfg.idle_for));
            if quiet.live_turns > 0 || recent_human {
                return Ok(Tick::Idle);
            }
        }
        let run = self.night(now, cancel).await?;
        Ok(Tick::Slept(run))
    }

    async fn skip(&self, due_at: Timestamp, reason: &str) -> DbResult<()> {
        let shared = self.kernel.shared();
        shared
            .db()
            .write(move |tx| tx.meta().set_timestamp(SLEEP_SKIPPED_AT, due_at))
            .await?;
        shared.bump(Metric::SleepSkipped, "").await;
        log::info!(reason; "sleep night skipped");
        Ok(())
    }

    /// Ночь: фазы 1 → 2 → 3, ошибка фазы не отменяет следующие; `cancel`
    /// между фазами — `interrupted`, оставшиеся не идут (`08 §2.6`).
    async fn night(&self, now: Timestamp, cancel: &CancellationToken) -> DbResult<RunId> {
        let shared = self.kernel.shared();
        let cfg = shared.cfg().sleep.clone();
        let llm = &shared.cfg().llm;
        let run: RunId = shared.make_world().mint();
        let chat = self.sleep_chat(now).await?;
        let dry_run = cfg.dry_run;
        {
            let run = run.clone();
            shared
                .db()
                .write(move |tx| tx.sleep().insert_run(&run, now, dry_run))
                .await?;
        }
        shared.bump(Metric::SleepRuns, "").await;
        let model = cfg
            .model
            .clone()
            .or_else(|| llm.roles.smartest.clone())
            .unwrap_or_else(|| llm.default_model.clone());
        let turn = |phase: Phase, prompt: String, reset: bool| PhaseTurn {
            run: run.clone(),
            phase,
            chat,
            reset,
            model: model.clone(),
            effort: cfg.effort,
            prompt,
            max_steps: cfg.max_steps,
            cancel: cancel.clone(),
        };
        let mut night = Night::default();
        let mut fresh = true;

        if cancel.is_cancelled() {
            night.interrupted = true;
        } else {
            let phase = memory::Phase::build(shared, run.clone(), now).await?;
            self.phase_started(&run, Phase::Memory, phase.as_ref().map(|_| &model))
                .await?;
            let (outcome, summary) = match phase {
                None => (
                    Outcome::Done {
                        report: format!(
                            "[skipped] corpus below sleep.min_memories = {}",
                            cfg.min_memories
                        ),
                    },
                    memory::Summary::default(),
                ),
                Some(phase) => {
                    let prompt = phase.prompt().to_owned();
                    let phase = Arc::new(tokio::sync::Mutex::new(phase));
                    let outcome = self
                        .engine
                        .memory(
                            turn(Phase::Memory, prompt, std::mem::replace(&mut fresh, false)),
                            phase.clone(),
                        )
                        .await;
                    let done = matches!(outcome, Outcome::Done { .. });
                    let phase = Arc::try_unwrap(phase)
                        .map_err(|_| {
                            DbError::Task(String::from("sleep phase still borrowed after turn"))
                        })?
                        .into_inner();
                    (outcome, phase.finish(done).await?)
                }
            };
            let report = night.phase(Phase::Memory, &outcome, &summary);
            self.phase_finished(&run, Phase::Memory, &summary, &report)
                .await?;
        }

        if cancel.is_cancelled() {
            night.interrupted = true;
        } else {
            self.phase_started(&run, Phase::Groups, Some(&model))
                .await?;
            let phase = groups::Phase::build(shared, run.clone(), now).await?;
            let prompt = phase.prompt().to_owned();
            let phase = Arc::new(tokio::sync::Mutex::new(phase));
            let outcome = self
                .engine
                .groups(
                    turn(Phase::Groups, prompt, std::mem::replace(&mut fresh, false)),
                    phase.clone(),
                )
                .await;
            let phase = Arc::try_unwrap(phase)
                .map_err(|_| DbError::Task("sleep group phase still borrowed after turn".into()))?
                .into_inner();
            let summary = phase.finish().await?;
            let report = night.phase(Phase::Groups, &outcome, &summary);
            self.phase_finished(&run, Phase::Groups, &summary, &report)
                .await?;
        }

        if cancel.is_cancelled() {
            night.interrupted = true;
        } else {
            self.phase_started(&run, Phase::Warm, None).await?;
            let outcome = match self.engine.recompute(now).await {
                Ok(warm) => Outcome::Done {
                    report: format!(
                        "chats {}, added {}, removed {}",
                        warm.chats, warm.added, warm.removed
                    ),
                },
                Err(error) => Outcome::Failed { error },
            };
            let summary = memory::Summary::default();
            let report = night.phase(Phase::Warm, &outcome, &summary);
            self.phase_finished(&run, Phase::Warm, &summary, &report)
                .await?;
        }

        let finished = shared.now();
        let report = night.report();
        let ok = !night.failed && !night.interrupted;
        {
            let (run, report) = (run.clone(), report.clone());
            shared
                .db()
                .write(move |tx| {
                    tx.sleep()
                        .finish_run(&run, finished, ok, night.interrupted, Some(&report))
                })
                .await?;
        }
        if night.interrupted {
            shared.bump(Metric::SleepInterrupted, "").await;
        }
        log::info!(run:% = run, ok, interrupted = night.interrupted, applied = night.applied, forgets = night.forgets; "sleep night finished");
        if dry_run || night.forgets > 0 || night.applied >= cfg.notify.min_applied {
            notify(
                &self.kernel,
                format!("sleep: {} ops, {} forgotten", night.applied, night.forgets),
                report,
                Some(chat),
            )
            .await;
        }
        Ok(run)
    }

    async fn phase_started(
        &self,
        run: &RunId,
        phase: Phase,
        model: Option<&ModelId>,
    ) -> DbResult<()> {
        let shared = self.kernel.shared();
        let (run_row, model, now) = (run.clone(), model.cloned(), shared.now());
        shared
            .db()
            .write(move |tx| {
                tx.sleep()
                    .insert_phase(&run_row, phase, now, model.as_ref())
            })
            .await?;
        self.publish(run, phase, PhaseState::Started);
        Ok(())
    }

    async fn phase_finished(
        &self,
        run: &RunId,
        phase: Phase,
        summary: &memory::Summary,
        report: &str,
    ) -> DbResult<()> {
        let shared = self.kernel.shared();
        let (run_row, summary, report, now) = (
            run.clone(),
            summary.clone(),
            report.to_owned(),
            shared.now(),
        );
        shared
            .db()
            .write(move |tx| {
                tx.sleep().finish_phase(
                    &run_row,
                    phase,
                    &PhaseRowEnd {
                        finished: now,
                        ops_json: &summary.ops_json,
                        applied: summary.applied,
                        discarded: summary.discarded,
                        report: Some(&report),
                    },
                )
            })
            .await?;
        self.publish(run, phase, PhaseState::Finished);
        Ok(())
    }

    fn publish(&self, run: &RunId, phase: Phase, state: PhaseState) {
        self.kernel.shared().bus().publish(
            Event::SleepPhase(SleepPhase {
                phase,
                run_id: run.clone(),
                state,
            }),
            None,
        );
    }

    /// Persistent `undeletable` чат `sleep` в папке `system/sleep`; id — в `kernel_meta`.
    async fn sleep_chat(&self, now: Timestamp) -> DbResult<ChatId> {
        let shared = self.kernel.shared();
        let known = shared
            .db()
            .reader()
            .await?
            .run(|c| {
                let id = c.meta().chat(SLEEP_CHAT_ID)?;
                DbResult::Ok(match id {
                    Some(id) if c.chats().get(id)?.is_some() => Some(id),
                    _ => None,
                })
            })
            .await?;
        if let Some(id) = known {
            return Ok(id);
        }
        shared
            .db()
            .write(move |tx| {
                let folder = tx
                    .folders()
                    .by_name(&SurfaceId::from(surfaces::SYSTEM), "sleep")?
                    .ok_or_else(|| DbError::corrupt("builtin folder system/sleep is missing"))?;
                let id = tx.chats().insert(&NewChat {
                    folder_id: folder.id,
                    kind: Kind::Persistent,
                    title: Some(String::from("sleep")),
                    project: None,
                    workdir: None,
                    workdir_on: None,
                    now,
                })?;
                tx.sleep().make_undeletable(id)?;
                tx.meta().set_chat(SLEEP_CHAT_ID, id)?;
                Ok(id)
            })
            .await
    }

    /// Компонент `Supervisor`: `Shutdown` посреди ночи отменяет текущий тур
    /// штатно и ждёт конца ночи.
    pub async fn run(self, mut rx: SlaveRx) -> eyre::Result<()> {
        if let Err(e) = self.recover().await {
            log::error!(error:% = e; "sleep recovery failed");
        }
        let cancel = CancellationToken::new();
        let mut ticks = tokio::time::interval(*self.kernel.shared().cfg().sleep.tick);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticks.tick() => {
                    let tick = self.tick(&cancel);
                    tokio::pin!(tick);
                    let (res, stopping) = tokio::select! {
                        res = &mut tick => (res, false),
                        _ = rx.recv() => {
                            cancel.cancel();
                            (tick.await, true)
                        }
                    };
                    if let Err(e) = res {
                        log::error!(error:% = e; "sleep tick failed");
                    }
                    if stopping {
                        break;
                    }
                }
                _ = rx.recv() => break,
            }
        }
        log::info!("sleep stopped");
        Ok(())
    }
}
