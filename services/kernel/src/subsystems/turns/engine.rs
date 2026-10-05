//! Тур — одна задача (`01 §10.1`): `open` → (`pre_step` → `infer` →
//! `settle` → `tools` → `next`)* → `close`. Состояние меняется только на
//! границах фаз; токен отмены проверяется перед каждой и ещё раз перед
//! `turn_ended` (`07 §8`). Каждый шаг рендерится заново из БД.

use std::sync::Arc;

use eva::time::Timestamp;

use eva_protocol::{
    errors::Aux,
    types::{
        ChatId, Effort, EventId, ModelId, SegmentId, Thinking, TurnId,
        chat::{Role, SegmentReason},
        model::ModelDecl,
        turn::{FeedEvent, TurnOutcome},
    },
    wire::{
        events::{Event as BusEvent, TurnEvent},
        hooks::BeforeTurn,
    },
};

use crate::{
    components::Components,
    db::{Db as _, DbResult, Queries as _, stats::Metric},
    shared::Shared,
    subsystems::{
        stats::{Recorder, TurnTally},
        tools::step::StepTools,
        turns::{
            ctx::Ctx,
            estimate::{self, Estimator},
            feed, gates, hooks,
            live::{ChatSlot, LiveTurn},
            log::{
                self, Append, EndedBy, Event, Loaded, MessagePayload, ParamsChangedPayload,
                Payload, ReminderPayload, SystemTextPayload, ToolsAddedPayload, TurnEndedPayload,
            },
            marker::SystemText,
            pending, plan, reminder,
        },
    },
};

/// Исход тура до записи `turn_ended`.
#[derive(Debug, Clone, PartialEq)]
pub struct Ending {
    pub outcome: TurnOutcome,
    pub by: Option<EndedBy>,
    pub error: Option<String>,
}

impl Ending {
    pub fn completed(by: Option<EndedBy>) -> Self {
        Self {
            outcome: TurnOutcome::Completed,
            by,
            error: None,
        }
    }

    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            outcome: TurnOutcome::Failed,
            by: None,
            error: Some(error.into()),
        }
    }

    pub fn cancelled(by: Option<EndedBy>) -> Self {
        Self {
            outcome: TurnOutcome::Cancelled,
            by,
            error: None,
        }
    }
}

impl From<Aux> for Ending {
    fn from(e: Aux) -> Self {
        Self::failed(e.to_string())
    }
}

impl From<crate::db::DbError> for Ending {
    fn from(e: crate::db::DbError) -> Self {
        Self::failed(e.to_string())
    }
}

/// Фаза кончилась продолжением тура или его исходом.
pub type Flow<T> = Result<T, Ending>;

/// Чем кончился шаг.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Ещё шаг: вызовы тулов, продолжение после обрыва, компакция.
    Step,
    /// `end_turn`: drain-проверка решает.
    Done,
}

pub struct Turn<C: Components> {
    pub shared: Shared<C>,
    pub slot: Arc<ChatSlot>,
    pub live: LiveTurn,
    pub ctx: Ctx,
    pub started_at: Timestamp,
    pub tally: Option<TurnTally<C>>,
    /// Номер текущего шага, с 1.
    pub step: u32,
    pub tools: StepTools,
    pub segment: SegmentId,
    /// `context_effort` до конца тура.
    pub effort: Option<Effort>,
    /// `context_compact` на ближайшей границе шага.
    pub compact_requested: bool,
    /// Индекс модели в цепочке `[ctx.model] ++ llm.fallback`.
    pub chain: usize,
    pub overflowed: bool,
    pub truncated: bool,
    /// Динамическая часть последнего reminder тура и его сегмент.
    pub reminded: Option<(SegmentId, String)>,
    /// План изменился шагом: reminder обновится.
    pub plan_changed: bool,
    /// Точный счёт `system+tools` сегмента (`context.count_prefix`).
    pub prefix_exact: Option<(SegmentId, u32)>,
    /// Последнее событие, отрендеренное в запрос текущего шага.
    pub watermark: EventId,
    /// Ворота чата, взятые drain-проверкой: `turn_ended` пишется под ними.
    pub held: Option<tokio::sync::OwnedMutexGuard<()>>,
    pub sleep: Option<Arc<tokio::sync::Mutex<crate::subsystems::sleep::memory::Phase<C>>>>,
    pub group_phase: Option<Arc<tokio::sync::Mutex<crate::subsystems::sleep::groups::Phase<C>>>>,
    pub sleep_report: Arc<tokio::sync::Mutex<Option<String>>>,
    pub max_steps: Option<u32>,
}

impl<C: Components> Turn<C> {
    pub fn new(
        shared: Shared<C>,
        slot: Arc<ChatSlot>,
        live: LiveTurn,
        ctx: Ctx,
        segment: SegmentId,
    ) -> Self {
        let started_at = shared.now();
        let tally = Recorder::new(&shared).tally(ctx.chat, started_at);
        Self {
            shared,
            slot,
            live,
            ctx,
            started_at,
            tally: Some(tally),
            step: 0,
            tools: StepTools::default(),
            segment,
            effort: None,
            compact_requested: false,
            chain: 0,
            overflowed: false,
            truncated: false,
            reminded: None,
            plan_changed: false,
            prefix_exact: None,
            watermark: EventId(0),
            held: None,
            sleep: None,
            group_phase: None,
            sleep_report: Arc::default(),
            max_steps: None,
        }
    }

    pub fn chat(&self) -> ChatId {
        self.ctx.chat
    }

    pub fn turn_id(&self) -> &TurnId {
        &self.live.turn
    }

    /// Цепочка моделей тура (`01 §10`).
    pub fn chain(&self) -> Vec<ModelId> {
        let mut chain = vec![self.ctx.model.clone()];
        for model in &self.shared.cfg().llm.fallback {
            if !chain.contains(model) {
                chain.push(model.clone());
            }
        }
        chain
    }

    pub fn model_id(&self) -> ModelId {
        self.chain()
            .get(self.chain)
            .cloned()
            .unwrap_or_else(|| self.ctx.model.clone())
    }

    pub fn params(&self) -> (ModelId, Effort, Thinking) {
        (
            self.model_id(),
            self.effort.unwrap_or(self.ctx.effort),
            self.ctx.thinking,
        )
    }

    /// Отмена, пришедшая в любую паузу, кончает тур на ближайшей границе.
    pub fn checkpoint(&self) -> Flow<()> {
        if self.live.cancel.is_cancelled() {
            return Err(Ending::cancelled(self.live.by.get().cloned()));
        }
        Ok(())
    }

    pub fn at(&self, step: Option<u32>) -> Append<'_> {
        Append {
            chat: self.ctx.chat,
            segment: self.segment,
            turn_id: Some(&self.live.turn),
            step,
            now: self.shared.now(),
        }
    }

    /// Append событий движка одной транзакцией в текущий сегмент.
    pub async fn append(&self, payloads: Vec<Payload>) -> Result<Vec<EventId>, Aux> {
        let (chat, segment, turn, step, now) = (
            self.ctx.chat,
            self.segment,
            self.live.turn.clone(),
            self.step,
            self.shared.now(),
        );
        let ids = self
            .shared
            .db()
            .write(move |tx| {
                let at = Append {
                    chat,
                    segment,
                    turn_id: Some(&turn),
                    step: Some(step),
                    now,
                };
                payloads
                    .iter()
                    .map(|p| log::append(tx, at, p))
                    .collect::<DbResult<Vec<_>>>()
            })
            .await?;
        if let Some(last) = ids.last() {
            self.slot.feed.note(*last);
        }
        Ok(ids)
    }

    pub async fn system_text(&self, marker: SystemText) -> Result<EventId, Aux> {
        let ids = self
            .append(vec![Payload::SystemText(SystemTextPayload::new(marker))])
            .await?;
        Ok(ids[0])
    }

    /// Лог открытого сегмента целиком.
    pub async fn load(&self) -> Result<Loaded, Aux> {
        let segment = self.segment;
        self.shared
            .db()
            .reader()
            .await?
            .run(move |c| log::load(c, segment, None))
            .await?
            .ok_or_else(|| Aux::Internal(format!("segment {segment} is gone")))
    }

    pub async fn alert(&self) {
        let chat = self.ctx.chat;
        if let Err(e) = self
            .shared
            .db()
            .write(move |tx| tx.turns().set_alerted(chat))
            .await
        {
            ::log::error!(chat:% = chat, error:% = e; "failed to alert the chat");
        }
    }

    pub fn tally(&mut self) -> &mut TurnTally<C> {
        self.tally.get_or_insert_with(|| {
            Recorder::new(&self.shared).tally(self.ctx.chat, self.started_at)
        })
    }

    /// Тур целиком: исход записан, лента и шина оповещены.
    pub async fn run(mut self) {
        let ending = match self.drive().await {
            Ok(never) => match never {},
            Err(ending) => ending,
        };
        self.close(ending).await;
    }

    async fn drive(&mut self) -> Flow<std::convert::Infallible> {
        self.open().await?;
        loop {
            self.pre_step().await?;
            self.checkpoint()?;
            match self.infer().await? {
                Next::Step => {}
                Next::Done => {
                    if !self.drain().await? {
                        return Err(Ending::completed(None));
                    }
                }
            }
        }
    }

    /// `open` (`01 §10.1`): `turn.started`, вход в master-тур, `before_turn`.
    async fn open(&mut self) -> Flow<()> {
        self.slot.set_ctx(self.ctx.clone());
        self.slot.keep_alive(None);
        super::integration::enter(&self.shared, &self.ctx).await?;
        self.shared.bus().publish(
            BusEvent::TurnStarted(self.turn_event(None)),
            Some(self.ctx.surface.clone()),
        );
        if self.ctx.master_turn {
            plan::purge_settled(&self.shared, self.ctx.chat).await?;
        }
        self.checkpoint()?;
        let content = self.first_content().await?;
        let input = BeforeTurn {
            ctx: self.ctx.wire(&self.live.turn),
            content,
        };
        let decision = tokio::select! {
            () = self.live.cancel.cancelled() => return Err(Ending::cancelled(self.live.by.get().cloned())),
            decision = hooks::chain(&self.shared, &self.ctx, &input) => decision,
        };
        if let hooks::Verdict::Cancel { module, reason } = decision {
            self.step = 0;
            self.system_text(SystemText::TurnCancelledByHook {
                module: module.clone(),
                reason,
            })
            .await?;
            return Err(Ending {
                outcome: TurnOutcome::Blocked,
                by: Some(EndedBy::Hook { module }),
                error: None,
            });
        }
        Ok(())
    }

    /// Блоки реплик тура для `before_turn`.
    async fn first_content(&self) -> Result<Vec<eva_protocol::types::content::Block>, Aux> {
        let loaded = self.load().await?;
        Ok(loaded
            .events
            .iter()
            .filter(|e| e.turn_id.as_ref() == Some(&self.live.turn))
            .filter_map(|e| match &e.payload {
                Payload::Message(m) if m.role == Role::User => Some(m.blocks.clone()),
                _ => None,
            })
            .flatten()
            .collect())
    }

    pub fn turn_event(&self, outcome: Option<TurnOutcome>) -> TurnEvent {
        TurnEvent {
            turn_id: self.live.turn.clone(),
            chat: self.ctx.chat,
            surface: self.ctx.surface.id.clone(),
            actor: self.ctx.actor.clone(),
            run_id: self.ctx.run_id.clone(),
            outcome,
        }
    }

    /// `pre_step` (`01 §10.1`): лимит шагов, отложенный reset и триггеры
    /// компакции, ожидание провайдера, `params_changed`, снимок тулов →
    /// `tools_added`, reminder.
    async fn pre_step(&mut self) -> Flow<()> {
        self.checkpoint()?;
        let work = &self.shared.cfg().work;
        let max = self
            .max_steps
            .unwrap_or(if self.ctx.master_turn && self.ctx.trusted_chat {
                work.max_steps_hard
            } else {
                work.max_steps
            });
        if self.step >= max {
            self.shared
                .bump(Metric::TurnsStepCap, self.ctx.surface.id.as_str())
                .await;
            return Err(Ending::completed(Some(EndedBy::StepLimit)));
        }
        self.step += 1;
        self.tally().note_step();
        crate::subsystems::work::agents::flush_inbox(
            &self.shared,
            self.ctx.chat,
            &self.live.turn,
            self.segment,
        )
        .await?;

        let model = self.wait_model().await?;
        self.maybe_compact(&model).await?;
        self.tools = gates::step_tools(&self.shared, &self.ctx).await?;
        self.checkpoint()?;

        let loaded = self.load().await?;
        let mut payloads = Vec::new();
        let (model_id, effort, thinking) = self.params();
        let last = loaded.events.iter().rev().find_map(|e| match &e.payload {
            Payload::ParamsChanged(p) => Some((p.model.clone(), p.effort, p.thinking)),
            _ => None,
        });
        if last != Some((model_id.clone(), effort, thinking)) {
            payloads.push(Payload::ParamsChanged(ParamsChangedPayload {
                model: model_id,
                effort,
                thinking,
            }));
        }
        let added = self.tools_added(&loaded);
        if !added.is_empty() {
            payloads.push(Payload::ToolsAdded(ToolsAddedPayload { tools: added }));
        }
        let steps_left = max.saturating_sub(self.step);
        if let Some(text) = reminder::next(self, steps_left).await? {
            payloads.push(Payload::Reminder(ReminderPayload { text }));
        }
        if !payloads.is_empty() {
            let ids = self.append(payloads.clone()).await?;
            for (id, payload) in ids.iter().zip(&payloads) {
                if let Payload::Reminder(r) = payload {
                    self.slot.feed.derived(
                        *id,
                        0,
                        FeedEvent::ItemCompleted {
                            item: feed::item(
                                feed::logged_id(*id, 0),
                                Some(&self.live.turn),
                                *id,
                                eva_protocol::types::turn::ItemStatus::Completed,
                                eva_protocol::types::turn::ItemBody::Reminder {
                                    text: r.text.clone(),
                                },
                            ),
                            replaces: None,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    /// Visible тулы снимка, которых нет в `tools[]` сегмента (`02 §4.3`).
    fn tools_added(&self, loaded: &Loaded) -> Vec<eva_protocol::types::generate::ToolSpec> {
        let mut present: std::collections::BTreeSet<&str> = loaded
            .snapshot
            .tools
            .iter()
            .filter(|t| !t.deferred)
            .map(|t| t.name.as_str())
            .collect();
        for event in &loaded.events {
            if let Payload::ToolsAdded(a) = &event.payload {
                present.extend(a.tools.iter().map(|t| t.name.as_str()));
            }
        }
        self.tools
            .specs
            .iter()
            .filter(|s| !present.contains(s.canonical.as_str()))
            .map(|s| s.spec(false))
            .collect()
    }

    /// Модель шага в реестре; нет — ждём до `llm.provider_wait`, затем как `Auth` (`01 §10`).
    async fn wait_model(&mut self) -> Flow<ModelDecl> {
        loop {
            let id = self.model_id();
            let wait = *self.shared.cfg().llm.provider_wait;
            let mut changes = self.shared.models_changed();
            if let Some(entry) = self.shared.models().get(&id) {
                return Ok(entry.decl.clone());
            }
            ::log::warn!(chat:% = self.ctx.chat, model:% = id; "no provider for the step model: waiting");
            let ready = tokio::select! {
                () = self.live.cancel.cancelled() => return Err(Ending::cancelled(self.live.by.get().cloned())),
                found = async {
                    loop {
                        if changes.changed().await.is_err() {
                            return false;
                        }
                        if self.shared.models().get(&id).is_some() {
                            return true;
                        }
                    }
                } => found,
                () = tokio::time::sleep(wait) => false,
            };
            if !ready {
                self.fallback(&format!("model {id} has no provider"), None)
                    .await?;
            }
        }
    }

    /// Следующая модель цепочки с окном не меньше оценки (`01 §10`); иначе — исход `failed`.
    pub async fn fallback(&mut self, reason: &str, estimate: Option<u32>) -> Flow<()> {
        let chain = self.chain();
        let models = self.shared.models();
        let next = (self.chain + 1..chain.len()).find(|&i| {
            models
                .get(&chain[i])
                .is_none_or(|m| estimate.is_none_or(|e| m.decl.context_window >= e))
        });
        let widest = (self.chain + 1..chain.len())
            .max_by_key(|&i| models.get(&chain[i]).map_or(0, |m| m.decl.context_window));
        self.alert().await;
        let Some(next) = next.or(widest) else {
            return Err(Ending::failed(format!(
                "{reason}; the fallback chain is exhausted"
            )));
        };
        ::log::warn!(chat:% = self.ctx.chat, from:% = self.model_id(), to:% = chain[next], reason; "falling back to the next model");
        self.shared
            .bump(Metric::InferenceFallbacks, chain[next].as_str())
            .await;
        self.chain = next;
        Ok(())
    }

    /// Компакция на границе шага (`01 §6.1`): отложенный reset, ручная,
    /// порог, выход, downshift. Все пары шага уже закрыты.
    async fn maybe_compact(&mut self, model: &ModelDecl) -> Flow<()> {
        let chat = self.ctx.chat;
        let (reset, compact_at) = self
            .shared
            .db()
            .reader()
            .await?
            .run(move |c| {
                DbResult::Ok((c.turns().reset_pending(chat)?, c.turns().compact_at(chat)?))
            })
            .await?;
        if reset {
            self.ctx.audience = self
                .shared
                .db()
                .reader()
                .await?
                .run(move |c| c.turns().audience(chat))
                .await?;
            self.slot.set_ctx(self.ctx.clone());
            return self.compact(SegmentReason::Reset, model).await;
        }
        if std::mem::take(&mut self.compact_requested) {
            return self.compact(SegmentReason::Manual, model).await;
        }
        let loaded = self.load().await?;
        let estimate = self.estimate(&loaded, model).await?;
        let cfg = &self.shared.cfg().context;
        let threshold = estimate::compact_threshold(model, compact_at.unwrap_or(cfg.compact_at));
        let no_room = estimate::max_tokens(model, estimate, cfg).is_err();
        if estimate < threshold && !no_room {
            return Ok(());
        }
        let previous = loaded.events.iter().rev().find_map(|e| match &e.payload {
            Payload::ParamsChanged(p) => Some(p.model.clone()),
            _ => None,
        });
        let downshift = previous.is_some_and(|prev| {
            prev != model.name
                && self
                    .shared
                    .models()
                    .get(&prev)
                    .is_some_and(|p| p.decl.context_window > model.context_window)
        });
        let reason = if downshift {
            SegmentReason::Downshift
        } else {
            SegmentReason::Compact
        };
        ::log::info!(chat:% = chat, estimate, threshold, no_room, reason:? = reason; "compaction triggered at the step boundary");
        self.compact(reason, model).await
    }

    /// Оценка запроса шага (`01 §9`): якорь и события после него, без якоря — рендер.
    pub async fn estimate(&mut self, loaded: &Loaded, model: &ModelDecl) -> Flow<u32> {
        let chat = self.ctx.chat;
        let model_id = model.name.clone();
        let (anchor, ratio) = self
            .shared
            .db()
            .reader()
            .await?
            .run(move |c| DbResult::Ok((c.turns().anchor(chat)?, c.turns().ratio(&model_id)?)))
            .await?;
        let cfg = &self.shared.cfg().context;
        let est = Estimator::new(ratio, cfg);
        let anchor = anchor.filter(|a| loaded.events.iter().any(|e| e.id == a.event));
        if anchor.is_some() {
            return Ok(estimate::estimate(
                &est,
                anchor,
                &loaded.events,
                &super::render::Rendered {
                    system: Vec::new(),
                    tools: Vec::new(),
                    messages: Vec::new(),
                },
                None,
            ));
        }
        let rendered = self.render(loaded, model).await?;
        let exact = self.prefix_exact(loaded, model, &rendered).await;
        Ok(estimate::estimate(
            &est,
            None,
            &loaded.events,
            &rendered,
            exact,
        ))
    }

    /// Точный `system+tools` один раз на сегмент, если провайдер умеет.
    async fn prefix_exact(
        &mut self,
        loaded: &Loaded,
        model: &ModelDecl,
        rendered: &super::render::Rendered,
    ) -> Option<u32> {
        if let Some((segment, tokens)) = self.prefix_exact
            && segment == loaded.row.id
        {
            return Some(tokens);
        }
        if !self.shared.cfg().context.count_prefix {
            return None;
        }
        let entry = self.shared.models().get(&model.name).cloned()?;
        let decl = self.shared.models().provider(&entry.provider)?;
        if !decl.count_tokens {
            return None;
        }
        let provider = self.shared.provider(&entry.provider)?;
        let req = eva_protocol::types::generate::GenerateRequest {
            model: model.name.clone(),
            system: rendered.system.clone(),
            tools: rendered.tools.clone(),
            messages: vec![eva_protocol::types::generate::Message::User {
                blocks: vec![eva_protocol::types::generate::Block::Text {
                    text: String::from("."),
                }],
            }],
            breakpoints: Vec::new(),
            cache: Default::default(),
            effort: None,
            thinking: None,
            max_tokens: 0,
            format: None,
            purpose: Default::default(),
        };
        use eva_core::Provider as _;
        let tokens = tokio::select! {
            () = self.live.cancel.cancelled() => return None,
            result = provider.count_tokens(req) => result.ok()?,
        };
        self.prefix_exact = Some((loaded.row.id, tokens));
        Some(tokens)
    }

    /// `end_turn`: реплика тура без `consumed_by`, пришедшая после запроса
    /// шага, — ещё шаг; иначе тур закрывается под воротами, и новая реплика
    /// стартует новый тур.
    async fn drain(&mut self) -> Flow<bool> {
        let gate = self.slot.gate.clone().lock_owned().await;
        let (turn, after) = (self.live.turn.clone(), self.watermark);
        let rows = self
            .shared
            .db()
            .reader()
            .await?
            .run(move |c| c.turns().turn_events_after(&turn, after))
            .await?;
        let events = log::decode_all(rows).map_err(Aux::from)?;
        let pending = events.iter().any(|e| {
            matches!(&e.payload, Payload::Message(m) if m.role == Role::User && m.consumed_by.is_none())
        });
        if !pending {
            self.held = Some(gate);
        }
        Ok(pending)
    }

    /// `close` (`01 §10.1`).
    async fn close(mut self, ending: Ending) {
        let Ending {
            mut outcome,
            mut by,
            error,
        } = ending;
        let error = error.map(|error| self.shared.turns().scrubber().scrub(&error).0.into_owned());
        // Отмена, пришедшая в последнюю паузу, называет источник.
        if self.live.cancel.is_cancelled() && outcome != TurnOutcome::Cancelled {
            outcome = TurnOutcome::Cancelled;
            by = self.live.by.get().cloned();
        }
        if matches!(outcome, TurnOutcome::Cancelled | TurnOutcome::Failed) {
            pending::cancel_turn(&self.shared, &self.slot, &self.live.turn).await;
        }
        if outcome == TurnOutcome::Failed {
            self.alert().await;
            ::log::error!(chat:% = self.ctx.chat, turn:% = self.live.turn, error:? = error; "turn failed");
        }

        let payload = Payload::TurnEnded(TurnEndedPayload {
            turn_id: self.live.turn.clone(),
            outcome,
            by: by.clone(),
            error: error.clone(),
        });
        let ended = {
            let _gate = match self.held.take() {
                Some(held) => held,
                None => self.slot.gate.clone().lock_owned().await,
            };
            let segment = self.current_segment().await.unwrap_or(self.segment);
            self.segment = segment;
            let ended = match self.append(vec![payload]).await {
                Ok(ids) => ids.last().copied(),
                Err(e) => {
                    ::log::error!(chat:% = self.ctx.chat, turn:% = self.live.turn, error:% = e; "failed to write turn_ended");
                    None
                }
            };
            self.slot.set_live(None);
            ended
        };

        let done = FeedEvent::TurnDone {
            turn: self.live.turn.clone(),
            outcome,
            error,
        };
        if let Some(id) = ended {
            self.slot.feed.derived(id, 0, done);
        } else {
            self.slot.feed.live(done);
        }
        self.shared.bus().publish(
            BusEvent::TurnFinished(self.turn_event(Some(outcome))),
            Some(self.ctx.surface.clone()),
        );
        let metric = match outcome {
            TurnOutcome::Completed | TurnOutcome::Blocked => Metric::TurnsCompleted,
            TurnOutcome::Cancelled => Metric::TurnsCancelled,
            _ => Metric::TurnsFailed,
        };
        self.shared.bump(metric, self.ctx.surface.id.as_str()).await;
        if let Some(tally) = self.tally.take()
            && let Err(e) = self.finish_tally(tally, outcome).await
        {
            ::log::error!(chat:% = self.ctx.chat, error:% = e; "failed to flush turn stats");
        }
        if let Err(e) = plan::delete_if_closed(&self.shared, self.ctx.chat).await {
            ::log::error!(chat:% = self.ctx.chat, error:% = e; "failed to drop a settled plan");
        }
        self.shared
            .ports()
            .work
            .on_turn_closed(&self.shared, self.ctx.chat, &self.live.turn, outcome)
            .await;
        if !self.live.cancel.is_cancelled() {
            super::keep_alive::schedule(&self.shared, &self.slot);
        }
        self.live.closed.cancel();
        ::log::info!(chat:% = self.ctx.chat, turn:% = self.live.turn, outcome:% = outcome, steps = self.step; "turn closed");
    }

    async fn finish_tally(&self, mut tally: TurnTally<C>, outcome: TurnOutcome) -> Result<(), Aux> {
        let turn = self.live.turn.clone();
        let events = self
            .shared
            .db()
            .reader()
            .await?
            .run(move |c| c.turns().turn_events_after(&turn, EventId(0)))
            .await?;
        for event in log::decode_all(events).map_err(Aux::from)? {
            if matches!(event.payload, Payload::Message(ref m) if m.role == Role::User && m.speaker.is_some())
            {
                tally.note_user_message();
            }
        }
        tally.finish(outcome == TurnOutcome::Cancelled).await?;
        Ok(())
    }

    /// Открытый сегмент чата: компакция могла его сменить.
    pub async fn current_segment(&self) -> Result<SegmentId, Aux> {
        let chat = self.ctx.chat;
        self.shared
            .db()
            .reader()
            .await?
            .run(move |c| c.turns().open_segment(chat))
            .await?
            .map(|s| s.id)
            .ok_or_else(|| Aux::Internal(format!("chat {chat} has no open segment")))
    }

    /// Последнее событие сегмента: граница «запрос видел» (`upto`).
    pub fn last_event(events: &[Event]) -> EventId {
        events.last().map_or(EventId(0), |e| e.id)
    }

    pub fn user_message(m: &MessagePayload) -> bool {
        m.role == Role::User
    }
}
