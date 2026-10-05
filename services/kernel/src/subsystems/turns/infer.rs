//! Фазы `infer` и `settle` (`01 §10`): рендер шага из БД, запрос к
//! провайдеру с дельтами в ленту, таблица ошибок, ретраи и fallback,
//! запись ответа одной транзакцией.

use std::time::Duration;

use futures::{StreamExt as _, channel::mpsc};

use eva_core::{GenSink, Provider as _};
use eva_protocol::types::{
    Bytes, EventId, ModelId,
    chat::{Role, SegmentReason},
    content::Block as Content,
    generate::{
        GenItem, GenerateRequest, ProviderError, ProviderErrorKind, Purpose, Stop, StopReason,
        Usage,
    },
    model::ModelDecl,
    stats::BreakReason,
    turn::{FeedEvent, ItemBody, ItemDelta, ItemStatus, StepUsage},
};
use serde_json::Value;

use crate::{
    components::Components,
    db::{
        Db as _, DbResult, Queries as _,
        blobs::Source,
        stats::{Metric, Op},
        turns::{Anchor, NewMessage},
    },
    subsystems::{
        stats::{Call, Recorder},
        tools::step::Named,
        turns::{
            blob, cache,
            engine::{Ending, Flow, Next, Turn},
            estimate, feed,
            live::LastRequest,
            log::{
                self, Append, Loaded, MessagePayload, Payload, Synthetic, ThinkingBlob,
                ThinkingPayload, ToolUsePayload,
            },
            marker::SystemText,
            render::{self, RenderInput, Rendered, RequestParams},
        },
    },
};

/// Что пришло от провайдера за один `generate`.
#[derive(Debug, Default)]
pub struct Generated {
    pub thinking: Vec<ThinkingBlob>,
    /// Незакрытый thinking: без подписи не реплеится.
    pub open_thinking: Option<String>,
    pub text: Vec<String>,
    /// Текущий текстовый блок ещё открыт.
    pub open_text: bool,
    pub uses: Vec<Use>,
    pub images: Vec<(String, Bytes)>,
    pub usage: Vec<Usage>,
    pub stop: Option<Stop>,
    pub error: Option<ProviderError>,
    /// Пришла хоть одна дельта: ретрай повторил бы наговорённое.
    pub delivered: bool,
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Use {
    pub id: String,
    pub name: String,
    pub input: Value,
}

impl<C: Components> Turn<C> {
    pub async fn render(&self, loaded: &Loaded, model: &ModelDecl) -> Flow<Rendered> {
        let blobs = blob::load(&self.shared, &loaded.events, model.caps.vision).await?;
        let tz = self.shared.day_zone().tz();
        Ok(render::render(RenderInput {
            segment: &loaded.snapshot,
            events: &loaded.events,
            caps: &model.caps,
            blobs: &blobs,
            tz: &tz,
        }))
    }

    fn model_decl(&self) -> Option<ModelDecl> {
        self.shared
            .models()
            .get(&self.model_id())
            .map(|e| e.decl.clone())
    }

    /// `infer` → `settle` → `tools` одного шага.
    pub async fn infer(&mut self) -> Flow<Next> {
        let Some(model) = self.model_decl() else {
            // Провайдер ушёл между фазами: следующий pre_step подождёт его.
            return Ok(Next::Step);
        };
        let loaded = self.load().await?;
        let rendered = self.render(&loaded, &model).await?;
        let estimate = self.estimate(&loaded, &model).await?;
        self.watermark = Turn::<C>::last_event(&loaded.events);
        let req = self.request(rendered.clone(), &model, estimate).await?;

        let retries = self.shared.cfg().llm.retries;
        let backoff = *self.shared.cfg().llm.retry_backoff;
        let mut attempt = 0_u32;
        let generated = loop {
            let generated = self.generate(req.clone(), &model).await?;
            if generated.cancelled {
                if generated.delivered {
                    self.settle(&generated, &model, &rendered, &req, true)
                        .await?;
                }
                return Err(Ending::cancelled(self.live.by.get().cloned()));
            }
            let transient = match &generated.error {
                Some(e) => matches!(
                    e.kind,
                    ProviderErrorKind::Transient | ProviderErrorKind::Other
                ),
                None => generated.stop.is_none(),
            };
            if transient && !generated.delivered && attempt < retries {
                self.shared
                    .bump(Metric::InferenceRetries, model.name.as_str())
                    .await;
                let wait = generated
                    .error
                    .as_ref()
                    .and_then(|e| e.retry_after)
                    .map_or_else(|| backoff.saturating_mul(1 << attempt.min(16)), |r| r.0);
                ::log::warn!(chat:% = self.ctx.chat, model:% = model.name, attempt, error:? = generated.error.as_ref().map(|e| e.to_string()); "transient inference failure: retrying");
                attempt += 1;
                tokio::select! {
                    () = self.live.cancel.cancelled() => return Err(Ending::cancelled(self.live.by.get().cloned())),
                    () = tokio::time::sleep(wait) => {}
                }
                continue;
            }
            break generated;
        };
        self.outcome(generated, &model, &rendered, &req, estimate)
            .await
    }

    /// Таблица `01 §10`.
    async fn outcome(
        &mut self,
        generated: Generated,
        model: &ModelDecl,
        rendered: &Rendered,
        req: &GenerateRequest,
        estimate: u32,
    ) -> Flow<Next> {
        let error = match (&generated.error, &generated.stop) {
            (Some(e), _) => Some(e.clone()),
            (None, None) => Some(ProviderError {
                kind: ProviderErrorKind::Other,
                http_status: None,
                raw: String::from("the stream ended without stop"),
                retry_after: None,
            }),
            (None, Some(_)) => None,
        };
        if let Some(error) = error {
            if !matches!(
                error.kind,
                ProviderErrorKind::Transient | ProviderErrorKind::Other
            ) {
                self.shared
                    .bump(Metric::InferenceErrors, error.kind.to_string().as_str())
                    .await;
            }
            return match error.kind {
                ProviderErrorKind::Transient | ProviderErrorKind::Other if generated.delivered => {
                    self.settle(&generated, model, rendered, req, true).await?;
                    self.system_text(SystemText::ResponseInterrupted).await?;
                    self.shared
                        .bump(Metric::InferencePartialRetry, model.name.as_str())
                        .await;
                    Ok(Next::Step)
                }
                ProviderErrorKind::Transient | ProviderErrorKind::Other => {
                    Err(Ending::failed(format!("inference failed: {error}")))
                }
                ProviderErrorKind::Auth
                | ProviderErrorKind::NotFound
                | ProviderErrorKind::Quota => {
                    self.fallback(&error.to_string(), Some(estimate)).await?;
                    Ok(Next::Step)
                }
                ProviderErrorKind::Overflow if !self.overflowed => {
                    self.overflowed = true;
                    self.compact(SegmentReason::Overflow, model).await?;
                    Ok(Next::Step)
                }
                ProviderErrorKind::Overflow => {
                    self.alert().await;
                    Err(Ending::failed(format!(
                        "context overflow after compaction: {error}"
                    )))
                }
                ProviderErrorKind::ContentFilter => {
                    self.system_text(SystemText::ContentFiltered).await?;
                    Err(Ending::completed(None))
                }
                ProviderErrorKind::Invalid => {
                    self.alert().await;
                    ::log::error!(chat:% = self.ctx.chat, model:% = model.name, status:? = error.http_status, raw = error.raw.as_str(); "provider rejected the request: a render or adapter bug");
                    Err(Ending::failed(format!("invalid request: {error}")))
                }
            };
        }

        let stop = generated
            .stop
            .clone()
            .map_or(StopReason::Other, |s| s.reason);
        if stop == StopReason::Refusal {
            self.system_text(SystemText::ModelRefused).await?;
            self.alert().await;
            self.shared
                .bump(Metric::InferenceRefusals, model.name.as_str())
                .await;
            return Err(Ending::completed(None));
        }
        let uses = self.settle(&generated, model, rendered, req, false).await?;
        if let Some(by) = self.dispatch(&uses).await? {
            return Err(Ending::completed(Some(by)));
        }
        if stop == StopReason::MaxTokens {
            if self.truncated {
                return Err(Ending::completed(None));
            }
            self.truncated = true;
            self.system_text(SystemText::OutputTruncated).await?;
            return Ok(Next::Step);
        }
        if uses.is_empty() {
            Ok(Next::Done)
        } else {
            Ok(Next::Step)
        }
    }

    /// Параметры шага поверх рендера (`01 §7`, `§8`).
    async fn request(
        &self,
        rendered: Rendered,
        model: &ModelDecl,
        estimate: u32,
    ) -> Flow<GenerateRequest> {
        let chat = self.ctx.chat;
        let cfg = &self.shared.cfg().context;
        let gap = self
            .shared
            .db()
            .reader()
            .await?
            .run(move |c| c.turns().gap(chat))
            .await?;
        if self.step == 1 {
            let next = cache::update_gap(gap, self.shared.now(), &cfg.cache);
            self.shared
                .db()
                .write(move |tx| tx.turns().set_gap(chat, next))
                .await?;
        }
        let ttl = cache::ttl(gap, model, &cfg.cache);
        let breakpoints =
            cache::place_breakpoints(&rendered, &model.caps, cfg.cache.breakpoint_every, estimate);
        let max_tokens = match estimate::max_tokens(model, estimate, cfg) {
            Ok(tokens) => tokens,
            Err(no) if no.room > 0 => no.room,
            Err(_) => {
                return Err(Ending::failed(
                    "context has no output room after compaction",
                ));
            }
        };
        let (model_id, effort, thinking) = self.params();
        Ok(rendered.into_request(
            RequestParams {
                model: model_id,
                effort,
                thinking,
                breakpoints,
                // Последний шаг тура не известен до ответа: стратегия gap — всем шагам.
                cache: cache::cache_mode(&model.caps, estimate, true, ttl),
                max_tokens,
                purpose: Purpose::Turn,
            },
            &model.caps,
        ))
    }

    /// Один `generate`: дельты — в ленту живыми item'ами, отмена бросает поток.
    async fn generate(&self, req: GenerateRequest, model: &ModelDecl) -> Flow<Generated> {
        let mut out = Generated::default();
        let entry = self.shared.models().get(&model.name).cloned();
        let provider = entry.and_then(|e| self.shared.provider(&e.provider));
        let Some(provider) = provider else {
            out.error = Some(ProviderError {
                kind: ProviderErrorKind::NotFound,
                http_status: None,
                raw: format!("model {} has no provider", model.name),
                retry_after: None,
            });
            return Ok(out);
        };
        let (tx, mut rx) = mpsc::channel::<GenItem>(64);
        let generate = provider.generate(req, GenSink::new(tx));
        tokio::pin!(generate);
        let mut finished: Option<Result<(), ProviderError>> = None;
        let mut drained = false;
        loop {
            if drained && let Some(res) = finished.take() {
                if let Err(e) = res {
                    out.error = Some(e);
                }
                break;
            }
            tokio::select! {
                biased;
                () = self.live.cancel.cancelled() => {
                    out.cancelled = true;
                    break;
                }
                item = rx.next(), if !drained => match item {
                    Some(item) => self.absorb(item, &mut out).await,
                    None => drained = true,
                },
                res = &mut generate, if finished.is_none() => finished = Some(res),
            }
        }
        out.close_open();
        Ok(out)
    }

    async fn absorb(&self, item: GenItem, out: &mut Generated) {
        let turn = &self.live.turn;
        match item {
            GenItem::TextDelta { text } => {
                let id = feed::live_id(turn, self.step, false);
                if !out.delivered_text() {
                    self.slot.feed.live(FeedEvent::ItemStarted {
                        item: feed::item(
                            id.clone(),
                            Some(turn),
                            self.slot.feed.last(),
                            ItemStatus::Running,
                            ItemBody::AgentMessage {
                                text: String::new(),
                                cost: None,
                            },
                        ),
                    });
                }
                out.delivered = true;
                if out.open_text {
                    if let Some(last) = out.text.last_mut() {
                        last.push_str(&text);
                    }
                } else {
                    out.text.push(text.clone());
                    out.open_text = true;
                }
                self.slot.feed.live(FeedEvent::ItemDelta {
                    id,
                    delta: ItemDelta::Text { text },
                });
            }
            GenItem::ThinkingDelta { text } => {
                let id = feed::live_id(turn, self.step, true);
                if out.thinking.is_empty() && out.open_thinking.is_none() {
                    self.slot.feed.live(FeedEvent::ItemStarted {
                        item: feed::item(
                            id.clone(),
                            Some(turn),
                            self.slot.feed.last(),
                            ItemStatus::Running,
                            ItemBody::Thinking {
                                text: String::new(),
                            },
                        ),
                    });
                }
                out.delivered = true;
                out.open_text = false;
                out.open_thinking
                    .get_or_insert_with(String::new)
                    .push_str(&text);
                if !text.is_empty() {
                    self.slot.feed.live(FeedEvent::ItemDelta {
                        id,
                        delta: ItemDelta::Text { text },
                    });
                }
            }
            GenItem::ThinkingBlock {
                signature,
                model: _,
            } => {
                let text = out.open_thinking.take();
                out.thinking.push(ThinkingBlob::Signed { signature, text });
            }
            GenItem::RedactedThinking { data, model: _ } => {
                out.open_thinking = None;
                out.thinking.push(ThinkingBlob::Redacted { data });
            }
            GenItem::ToolUse { id, name, input } => {
                out.delivered = true;
                out.open_text = false;
                out.uses.push(Use { id, name, input });
            }
            GenItem::Image { media_type, data } => {
                out.delivered = true;
                out.open_text = false;
                out.images.push((media_type, data));
            }
            GenItem::Usage(usage) => out.usage.push(usage),
            GenItem::Stop(stop) => out.stop = Some(stop),
            GenItem::Unknown => {}
        }
    }

    /// `settle` (`01 §10.1`): thinking, `message{assistant}`, `tool_use` одной
    /// транзакцией; оборванный ответ (`partial`) закрывает свои вызовы
    /// `not_started`. Затем якорь, ratio, `spend`, лента.
    async fn settle(
        &mut self,
        generated: &Generated,
        model: &ModelDecl,
        rendered: &Rendered,
        req: &GenerateRequest,
        partial: bool,
    ) -> Flow<Vec<Use>> {
        let mut blocks: Vec<Content> = generated
            .text
            .iter()
            .filter(|t| !t.is_empty())
            .map(|t| Content::text(t.clone()))
            .collect();
        for (media_type, data) in &generated.images {
            match blob::store(
                &self.shared,
                self.ctx.chat,
                media_type,
                data,
                Source::Generated,
            )
            .await
            {
                Ok(blob) => blocks.push(Content::Image {
                    blob,
                    media_type: media_type.clone(),
                }),
                Err(e) => {
                    ::log::warn!(chat:% = self.ctx.chat, error:% = e; "generated image is dropped")
                }
            }
        }
        let model_id: ModelId = req.model.clone();
        let mut payloads: Vec<Payload> = generated
            .thinking
            .iter()
            .map(|blob| {
                Payload::Thinking(ThinkingPayload {
                    model: model_id.clone(),
                    blob: blob.clone(),
                })
            })
            .collect();
        payloads.push(Payload::Message(MessagePayload::assistant(
            model_id.clone(),
            blocks.clone(),
            (!partial)
                .then(|| generated.stop.clone().map(|s| s.reason))
                .flatten(),
            partial,
            self.watermark,
        )));
        let uses = generated.uses.clone();
        for u in &uses {
            let tool_id = match self.tools.names.get(&u.name) {
                Some(Named::Visible(i)) => self.tools.specs[*i].tool_id,
                Some(Named::Reserved(id)) => Some(*id),
                None => None,
            };
            payloads.push(Payload::ToolUse(ToolUsePayload {
                call_id: u.id.clone(),
                tool_id,
                name: u.name.clone(),
                input: u.input.clone(),
            }));
        }

        let usage = sum(&generated.usage);
        let cfg = self.shared.cfg().context.clone();
        let (text_bytes, images) = estimate::text_bytes(rendered);
        let (chat, segment, turn, step, now) = (
            self.ctx.chat,
            self.segment,
            self.live.turn.clone(),
            self.step,
            self.shared.now(),
        );
        let has_usage = !generated.usage.is_empty();
        let model_name = model.name.clone();
        let unstarted: Vec<String> = if partial {
            uses.iter().map(|u| u.id.clone()).collect()
        } else {
            Vec::new()
        };
        let written = self
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
                let mut ids = Vec::with_capacity(payloads.len());
                let mut message = EventId(0);
                for payload in &payloads {
                    let id = log::append(tx, at, payload)?;
                    if matches!(payload, Payload::Message(_)) {
                        message = id;
                        tx.turns().insert_message(&NewMessage {
                            chat,
                            event: id,
                            role: Role::Assistant,
                            sender: None,
                            external_id: None,
                            reply_to_external_id: None,
                            turn_id: Some(&turn),
                        })?;
                    }
                    ids.push(id);
                }
                let mut synthetic = Vec::new();
                for call_id in &unstarted {
                    synthetic.push(super::dispatch::write_synthetic(
                        tx,
                        at,
                        call_id,
                        None,
                        Synthetic::NotStarted,
                    )?);
                }
                if has_usage {
                    tx.turns().set_anchor(
                        chat,
                        Anchor {
                            event: message,
                            tokens: estimate::anchor_tokens(&usage),
                        },
                    )?;
                    let ratio = tx.turns().ratio(&model_name)?.unwrap_or(cfg.ratio_default);
                    if let Some(next) = estimate::calibrate(ratio, text_bytes, images, &usage, &cfg)
                    {
                        tx.turns().set_ratio(&model_name, next, now)?;
                    }
                }
                DbResult::Ok((ids, message, synthetic))
            })
            .await?;
        let (ids, message, synthetic) = written;
        if let Some(last) = ids.last() {
            self.slot.feed.note(*last);
        }
        self.tally().note_assistant_message();
        self.feed_settled(generated, &ids, message, &blocks, &uses, partial);
        for (call, result) in uses.iter().zip(&synthetic) {
            self.feed_result(call, result);
        }
        self.spend(generated, model, req).await;
        Ok(if partial { Vec::new() } else { uses })
    }

    fn feed_settled(
        &self,
        generated: &Generated,
        ids: &[EventId],
        message: EventId,
        blocks: &[Content],
        uses: &[Use],
        partial: bool,
    ) {
        let turn = &self.live.turn;
        let status = if partial {
            ItemStatus::Aborted
        } else {
            ItemStatus::Completed
        };
        let thinking = ids.iter().take(generated.thinking.len());
        for (n, (id, blob)) in thinking.zip(&generated.thinking).enumerate() {
            let text = match blob {
                ThinkingBlob::Signed { text, .. } => text.clone().unwrap_or_default(),
                ThinkingBlob::Redacted { .. } => String::new(),
            };
            self.slot.feed.derived(
                *id,
                0,
                FeedEvent::ItemCompleted {
                    item: feed::item(
                        feed::logged_id(*id, 0),
                        Some(turn),
                        *id,
                        status,
                        ItemBody::Thinking { text },
                    ),
                    replaces: (n == 0).then(|| feed::live_id(turn, self.step, true)),
                },
            );
        }
        let bodies = std::iter::once(ItemBody::AgentMessage {
            text: super::items::text(blocks),
            cost: None,
        })
        .chain(super::items::attachments(blocks));
        for (n, body) in bodies.enumerate() {
            self.slot.feed.derived(
                message,
                u32::try_from(n).unwrap_or(u32::MAX),
                FeedEvent::ItemCompleted {
                    item: feed::item(
                        feed::logged_id(message, n),
                        Some(turn),
                        message,
                        status,
                        body,
                    ),
                    replaces: (n == 0 && generated.delivered_text())
                        .then(|| feed::live_id(turn, self.step, false)),
                },
            );
        }
        let uses_ids = ids.iter().skip(generated.thinking.len() + 1);
        for (id, u) in uses_ids.zip(uses) {
            let body = ItemBody::ToolCall {
                call_id: u.id.clone(),
                name: u.name.clone(),
                input: u.input.clone(),
                agent: None,
                result: None,
                progress: None,
            };
            let item = feed::item(u.id.clone(), Some(turn), *id, ItemStatus::Running, body);
            self.slot
                .feed
                .derived(*id, 0, FeedEvent::ItemStarted { item });
        }
    }

    /// Строка `spend` и item `usage`; разрыв кэша — `01 §11`.
    async fn spend(&self, generated: &Generated, model: &ModelDecl, req: &GenerateRequest) {
        let usage = sum(&generated.usage);
        let prompt =
            usage.input + usage.cache_read + usage.cache_write_short + usage.cache_write_long;
        let now = self.shared.now();
        let prev = self.slot.last_request();
        let tools: Vec<String> = req.tools.iter().map(|t| t.name.clone()).collect();
        let reason = break_reason(
            prev.as_ref(),
            req,
            model,
            &tools,
            self.segment,
            usage.cache_read,
            self.shared.cfg().stats.cache.break_ratio,
            now,
        );
        let chat = self.ctx.chat;
        let gap = match self.shared.db().reader().await {
            Ok(reader) => reader.run(move |c| c.turns().gap(chat)).await.ok(),
            Err(_) => None,
        };
        let ttl = gap.map_or(cache::Ttl::Short, |gap| {
            cache::ttl(gap, model, &self.shared.cfg().context.cache)
        });
        self.slot.set_last_request(LastRequest {
            request: req.clone(),
            model: req.model.clone(),
            effort: req.effort.unwrap_or(self.params().1),
            thinking: req.thinking.unwrap_or(self.ctx.thinking),
            segment: self.segment,
            tools,
            at: now,
            prompt,
            context_edits: usage.context_edits.applied,
            ttl,
        });
        let call = Call {
            model: model.name.clone(),
            origin: self.ctx.source.clone(),
            op: Op::Step,
            chat: Some(self.ctx.chat),
            turn: Some(self.live.turn.clone()),
            run: self.ctx.run_id.clone(),
            cache_break_reason: reason,
        };
        if let Err(e) = Recorder::new(&self.shared)
            .spend(call, &generated.usage)
            .await
        {
            ::log::error!(chat:% = self.ctx.chat, error:% = e; "failed to record spend");
        }
        if !generated.usage.is_empty() {
            self.slot.feed.live(FeedEvent::Usage(StepUsage {
                turn: self.live.turn.clone(),
                step: self.step,
                model: model.name.clone(),
                input: usage.input,
                cache_read: usage.cache_read,
                cache_write: usage.cache_write_short + usage.cache_write_long,
                output: usage.output,
                cost: usage.cost,
            }));
        }
    }
}

impl Generated {
    fn delivered_text(&self) -> bool {
        !self.text.is_empty()
    }

    /// Незакрытый thinking без подписи хранится, но не реплеится (`01 §1.2`).
    fn close_open(&mut self) {
        if let Some(text) = self.open_thinking.take() {
            self.thinking.push(ThinkingBlob::Signed {
                signature: Bytes(Vec::new()),
                text: Some(text),
            });
        }
    }
}

pub fn sum(chunks: &[Usage]) -> Usage {
    let mut total = Usage::default();
    for u in chunks {
        total.input += u.input;
        total.cache_read += u.cache_read;
        total.cache_write_short += u.cache_write_short;
        total.cache_write_long += u.cache_write_long;
        total.output += u.output;
        total.reasoning += u.reasoning;
        total.context_edits.applied += u.context_edits.applied;
        total.context_edits.cleared_input_tokens += u.context_edits.cleared_input_tokens;
        total.cost = match (total.cost, u.cost) {
            (Some(a), Some(b)) => Some(a + b),
            (a, b) => a.or(b),
        };
    }
    total
}

/// `cache_break_reason` (`01 §11`): только при `cache_read < break_ratio × prefix_prev`.
#[allow(clippy::too_many_arguments)]
pub fn break_reason(
    prev: Option<&LastRequest>,
    req: &GenerateRequest,
    model: &ModelDecl,
    tools: &[String],
    segment: eva_protocol::types::SegmentId,
    cache_read: u64,
    break_ratio: f32,
    now: eva::time::Timestamp,
) -> Option<BreakReason> {
    if req.cache == eva_protocol::types::generate::CacheMode::Off {
        return None;
    }
    let Some(prev) = prev else {
        return Some(BreakReason::FirstRequest);
    };
    if (cache_read as f64) >= f64::from(break_ratio) * prev.prompt as f64 {
        return None;
    }
    let ttl = match prev.ttl {
        cache::Ttl::Long => Duration::from_secs(60 * 60),
        cache::Ttl::Short | cache::Ttl::KeepAlive => Duration::from_secs(5 * 60),
    };
    let gap = now
        .as_nanos()
        .get()
        .saturating_sub(prev.at.as_nanos().get());
    Some(if prev.model != req.model {
        BreakReason::ModelChange
    } else if prev.segment != segment {
        BreakReason::SegmentReset
    } else if prev.tools.as_slice() != tools {
        BreakReason::ToolsChange
    } else if u128::from(gap) > ttl.as_nanos() {
        BreakReason::TtlExpired
    } else if prev.context_edits > 0 {
        BreakReason::ContextEdit
    } else if req.thinking.is_some_and(|t| t != prev.thinking) {
        BreakReason::ThinkingChange
    } else if req.effort.is_some_and(|e| e != prev.effort) && !model.caps.effort_in_messages {
        BreakReason::EffortChange
    } else {
        BreakReason::Unknown
    })
}
