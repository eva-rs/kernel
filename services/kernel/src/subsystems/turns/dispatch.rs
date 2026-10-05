//! Фаза `tools` (`02 §4.2`): вызовы одного ответа модели. Имя → снимок
//! шага (I3), Reserved — отказ, вход по схеме, `before_tool`, исполнение с
//! таймаутом, `after_tool`, скраб, окно, дедуп, запись пары. Подряд идущие
//! `parallel_safe` — одновременно, прочие — по порядку ответа; все пары
//! закрываются в этом шаге, отменой и `Fatal` тоже (`07 §8`).

use std::{collections::BTreeSet, time::Duration};

use futures::{StreamExt as _, stream::FuturesUnordered};

use eva::time::Timestamp;
use eva_protocol::{
    errors::Aux,
    types::{
        ToolId, Tseq,
        tool::{ResultForm, ToolContent, ToolDecl},
        turn::{FeedEvent, ItemBody, ItemDelta, ItemStatus, ToolCallResult},
    },
    wire::{
        events::{Event as BusEvent, ToolCalled},
        hooks::{AfterTool, BeforeTool},
        tools::{ToolCall, ToolItem, ToolResult},
    },
};
use serde_json::Value;

use crate::{
    components::Components,
    db::{
        Connection, Db as _, DbResult, Queries as _, blobs::Source, stats::Metric,
        turns::NewToolResult,
    },
    subsystems::{
        tools::{
            schema,
            step::{Named, StepSpec},
        },
        turns::{
            blob, dedup,
            engine::{Ending, Flow, Turn},
            estimate::Estimator,
            feed, hooks,
            infer::Use,
            kernel_tools::{self, Effects},
            log::{self, Append, EndedBy, Payload, ResultContent, Synthetic, ToolResultPayload},
            marker,
        },
    },
};

/// Решение до исполнения.
enum Plan {
    Run(Box<StepSpec>, Value),
    /// `is_error` без исполнения: неизвестное имя, Reserved, вход.
    Refuse(String),
    Synthetic(Synthetic),
}

/// Исход одного вызова.
enum Exec {
    Done {
        result: ToolResult,
        effects: Effects,
        started: Timestamp,
    },
    Fatal(String),
    Cancelled,
    Interrupted,
    Refused(String),
    Synthetic(Synthetic),
    NotStarted,
}

impl<C: Components> Turn<C> {
    /// `Some(by)` — успешный `ends_turn`: первый в порядке ответа (`02 §6`).
    pub async fn dispatch(&mut self, uses: &[Use]) -> Flow<Option<EndedBy>> {
        if uses.is_empty() {
            return Ok(None);
        }
        let rendered = self.rendered_names().await?;
        let plans: Vec<Plan> = uses.iter().map(|u| self.plan(u, &rendered)).collect();
        for plan in &plans {
            let reason = match plan {
                Plan::Refuse(text) if text.contains("is reserved") => Some("reserved"),
                Plan::Refuse(text) if text.starts_with("unknown tool") => Some("unknown"),
                Plan::Synthetic(Synthetic::ToolUnavailable) => Some("forbidden"),
                _ => None,
            };
            if let Some(reason) = reason {
                self.shared.bump(Metric::ToolsDenied, reason).await;
            }
        }

        let max = usize::from(self.shared.cfg().tools.parallel.max.max(1));
        let mut execs: Vec<Option<Exec>> = (0..uses.len()).map(|_| None).collect();
        let mut i = 0;
        let mut fatal = false;
        while i < uses.len() {
            let parallel = |p: &Plan| matches!(p, Plan::Run(spec, _) if spec.decl.parallel_safe);
            let end = if parallel(&plans[i]) {
                (i..uses.len())
                    .take_while(|&j| parallel(&plans[j]))
                    .take(max)
                    .last()
                    .map_or(i + 1, |j| j + 1)
            } else {
                i + 1
            };
            if fatal || self.live.cancel.is_cancelled() {
                for slot in &mut execs[i..end] {
                    *slot = Some(Exec::NotStarted);
                }
                i = end;
                continue;
            }
            let stop = tokio_util::sync::CancellationToken::new();
            let mut group: FuturesUnordered<_> = (i..end).map(|j| {
                let stop = stop.clone();
                let plan = &plans[j];
                let call = &uses[j];
                let turn = &*self;
                async move {
                    let exec = if matches!(plan, Plan::Run(spec, _) if spec.decl.waits_on_cancel) {
                        turn.exec(call, plan).await
                    } else {
                        tokio::select! {
                            biased;
                            () = stop.cancelled() => Exec::Interrupted,
                            exec = turn.exec(call, plan) => exec,
                        }
                    };
                    (j, exec)
                }
            }).collect();
            while let Some((j, exec)) = group.next().await {
                if matches!(exec, Exec::Fatal(_)) {
                    fatal = true;
                    stop.cancel();
                }
                execs[j] = Some(exec);
            }
            i = end;
        }

        let mut ends: Option<EndedBy> = None;
        let mut fatal_error: Option<String> = None;
        let mut effects = Effects::default();
        for ((u, plan), exec) in uses.iter().zip(&plans).zip(execs) {
            let exec = exec.unwrap_or(Exec::NotStarted);
            let spec = match plan {
                Plan::Run(spec, _) => Some(spec.as_ref()),
                _ => None,
            };
            if let Exec::Fatal(e) = &exec {
                fatal_error.get_or_insert_with(|| e.clone());
            }
            if let Exec::Done { effects: e, .. } = &exec {
                effects.merge(e);
            }
            let success = self.record(u, spec, exec).await?;
            if success
                && ends.is_none()
                && let Some(spec) = spec.filter(|s| s.decl.ends_turn)
            {
                ends = Some(EndedBy::Tool {
                    tool_id: spec.tool_id,
                    canonical: spec.canonical.clone(),
                });
            }
        }
        self.apply(effects);
        if let Some(error) = fatal_error {
            let (error, _) = self.shared.turns().scrubber().scrub(&error);
            return Err(Ending::failed(format!("tool failed: {error}")));
        }
        self.checkpoint()?;
        Ok(ends)
    }

    /// Имена отрендеренного `tools[]` сегмента (`01 §2.3` п.2).
    async fn rendered_names(&self) -> Result<BTreeSet<String>, Aux> {
        let loaded = self.load().await?;
        let caps = self
            .shared
            .models()
            .get(&self.model_id())
            .map(|m| m.decl.caps.clone())
            .unwrap_or_default();
        Ok(
            super::render::tools(&loaded.snapshot, &loaded.events, &caps)
                .into_iter()
                .map(|t| t.name)
                .collect(),
        )
    }

    fn plan(&self, u: &Use, rendered: &BTreeSet<String>) -> Plan {
        match self.tools.names.get(&u.name) {
            Some(Named::Visible(i)) => {
                let spec = self.tools.specs[*i].clone();
                match schema::check(u.input.clone(), &spec.decl.input) {
                    Ok(input) => Plan::Run(Box::new(spec), input),
                    Err(e) => Plan::Refuse(e),
                }
            }
            Some(Named::Reserved(id)) => {
                let path = self
                    .shared
                    .tools()
                    .get()
                    .get(*id)
                    .map_or_else(|| u.name.clone(), |e| e.path().to_owned());
                Plan::Refuse(format!(
                    "tool \"{}\" is reserved; call tools_load {{\"paths\":[\"{path}\"]}} first",
                    u.name
                ))
            }
            None if rendered.contains(&u.name) => Plan::Synthetic(Synthetic::ToolUnavailable),
            None => {
                let nearest = self.tools.nearest(&u.name);
                let hint = if nearest.is_empty() {
                    String::new()
                } else {
                    format!("; did you mean: {}", nearest.join(", "))
                };
                Plan::Refuse(format!(
                    "unknown tool \"{}\"{hint}; search with tools_find",
                    u.name
                ))
            }
        }
    }

    async fn exec(&self, u: &Use, plan: &Plan) -> Exec {
        let (spec, input) = match plan {
            Plan::Run(spec, input) => (spec.as_ref(), input.clone()),
            Plan::Refuse(text) => return Exec::Refused(text.clone()),
            Plan::Synthetic(kind) => return Exec::Synthetic(*kind),
        };
        let ctx = self.ctx.wire(&self.live.turn);
        let before = BeforeTool {
            ctx: ctx.clone(),
            tool_id: spec.tool_id,
            canonical: spec.canonical.clone(),
            input: input.clone(),
        };
        let verdict = tokio::select! {
            () = self.live.cancel.cancelled() => return Exec::NotStarted,
            verdict = hooks::chain(&self.shared, &self.ctx, &before) => verdict,
        };
        if let hooks::Verdict::Cancel { module, reason } = verdict {
            return Exec::Refused(marker::tool_blocked(&module, &reason));
        }
        let started = self.shared.now();
        let mut timeout = spec
            .decl
            .timeout
            .map_or(*self.shared.cfg().tools.call_timeout, |t| t.0);
        if spec
            .module
            .as_ref()
            .is_some_and(|m| m.as_str() == crate::db::tools::KERNEL)
            && (spec.route.starts_with("fs_") || spec.route.starts_with("shell_"))
        {
            timeout = timeout.saturating_add(*self.shared.cfg().work.permission_ttl);
        }
        let work = async {
            match tokio::time::timeout(timeout, self.invoke_tool(u, spec, input.clone())).await {
                Ok(exec) => exec,
                Err(_) => Exec::Done {
                    result: ToolResult::error(marker::TOOL_TIMED_OUT),
                    effects: Effects::default(),
                    started,
                },
            }
        };
        let exec = if spec.decl.waits_on_cancel {
            work.await
        } else {
            tokio::select! {
                exec = work => exec,
                () = self.live.cancel.cancelled() => Exec::Cancelled,
            }
        };
        match exec {
            Exec::Done {
                result, effects, ..
            } => {
                let result = self.after_tool(spec, &input, result).await;
                Exec::Done {
                    result,
                    effects,
                    started,
                }
            }
            other => other,
        }
    }

    /// Исполнитель по снимку: ядро, модуль реестра или сессия `turns.start`.
    async fn invoke_tool(&self, u: &Use, spec: &StepSpec, input: Value) -> Exec {
        let started = self.shared.now();
        if spec.tool_id.is_some()
            && spec
                .module
                .as_ref()
                .is_some_and(|m| m.as_str() == crate::db::tools::KERNEL)
        {
            return match kernel_tools::call(self, spec, &u.id, input).await {
                Ok((result, effects)) => Exec::Done {
                    result,
                    effects,
                    started,
                },
                Err(e) => Exec::Fatal(e.to_string()),
            };
        }
        let client = match (&spec.module, &self.ctx.origin) {
            (Some(module), _) => self.shared.live().client(module).map(|(_, c)| c),
            (None, Some((module, session))) => self
                .shared
                .live()
                .client(module)
                .filter(|(current, _)| current == session)
                .map(|(_, c)| c),
            (None, None) => None,
        };
        let Some(client) = client else {
            return Exec::Synthetic(Synthetic::ToolUnavailable);
        };
        let call = ToolCall {
            call_id: u.id.clone(),
            tool_id: spec.tool_id,
            canonical: spec.canonical.clone(),
            input,
            ctx: self.ctx.wire(&self.live.turn),
        };
        let mut stream = match client.call_tool(&call).await {
            Ok(stream) => stream,
            Err(e) => return Exec::Fatal(format!("{}: {e}", spec.canonical)),
        };
        while let Some(item) = stream.next().await {
            match item {
                Ok(ToolItem::Progress { text }) => {
                    let (text, _) = self.shared.turns().scrubber().scrub(&text);
                    self.slot.feed.live(FeedEvent::ItemDelta {
                        id: u.id.clone(),
                        delta: ItemDelta::Progress {
                            text: text.into_owned(),
                        },
                    });
                }
                Ok(ToolItem::Result(result)) => {
                    return Exec::Done {
                        result,
                        effects: Effects::default(),
                        started,
                    };
                }
                Ok(ToolItem::Unknown) => {}
                Err(e) => return Exec::Fatal(format!("{}: {e}", spec.canonical)),
            }
        }
        Exec::Fatal(format!(
            "{}: the tool stream ended without a result",
            spec.canonical
        ))
    }

    async fn after_tool(&self, spec: &StepSpec, input: &Value, result: ToolResult) -> ToolResult {
        let after = AfterTool {
            ctx: self.ctx.wire(&self.live.turn),
            tool_id: spec.tool_id,
            canonical: spec.canonical.clone(),
            input: input.clone(),
            result,
        };
        match hooks::chain(&self.shared, &self.ctx, &after).await {
            hooks::Verdict::Continue(after) => after.result,
            hooks::Verdict::Cancel { module, reason } => {
                ToolResult::error(marker::tool_blocked(&module, &reason))
            }
        }
    }

    /// Пара закрывается в логе; `true` — успешный вызов (`!is_error`).
    async fn record(&mut self, u: &Use, spec: Option<&StepSpec>, exec: Exec) -> Flow<bool> {
        let tool_id = spec.and_then(|s| s.tool_id);
        let canonical = spec.map_or_else(|| u.name.clone(), |s| s.canonical.clone());
        let (content, is_error, synthetic, executed, started) = match exec {
            Exec::Done {
                result, started, ..
            } => (result.content, result.is_error, None, true, Some(started)),
            Exec::Refused(text) => (vec![ToolContent::text(text)], true, None, false, None),
            Exec::Synthetic(kind) => (Vec::new(), true, Some(kind), false, None),
            Exec::Fatal(_) => (Vec::new(), true, Some(Synthetic::Interrupted), true, None),
            Exec::Interrupted => (Vec::new(), true, Some(Synthetic::Interrupted), true, None),
            Exec::Cancelled => (Vec::new(), true, Some(Synthetic::Cancelled), true, None),
            Exec::NotStarted => (Vec::new(), true, Some(Synthetic::NotStarted), false, None),
        };
        let written = match synthetic {
            Some(kind) => self.write_synthetic_pair(&u.id, tool_id, kind).await?,
            None => {
                self.write_result(
                    u,
                    tool_id,
                    &canonical,
                    spec.map(|s| &s.decl),
                    content,
                    is_error,
                )
                .await?
            }
        };
        self.feed_result(u, &written);
        if executed {
            if let Some(id) = tool_id {
                let ends = spec.is_some_and(|s| s.decl.ends_turn);
                self.tally().note_call(id, is_error, ends);
            }
            let duration = started.map_or(Duration::ZERO, |s| {
                Duration::from_nanos(
                    self.shared
                        .now()
                        .as_nanos()
                        .get()
                        .saturating_sub(s.as_nanos().get()),
                )
            });
            self.shared.bus().publish(
                BusEvent::ToolCalled(ToolCalled {
                    turn_id: self.live.turn.clone(),
                    chat: self.ctx.chat,
                    tool_id,
                    canonical,
                    is_error,
                    duration: eva::time::HumanDuration(duration),
                }),
                Some(self.ctx.surface.clone()),
            );
        }
        Ok(!is_error)
    }

    async fn write_synthetic_pair(
        &self,
        call_id: &str,
        tool_id: Option<ToolId>,
        kind: Synthetic,
    ) -> Flow<Written> {
        let (chat, segment, turn, step, now) = self.coords();
        let call_id = call_id.to_owned();
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
                write_synthetic(tx, at, &call_id, tool_id, kind)
            })
            .await?;
        self.slot.feed.note(written.event);
        Ok(written)
    }

    /// Скраб → окно → дедуп → событие и строка `tool_results`; картинки — в blob-store до события.
    async fn write_result(
        &self,
        u: &Use,
        tool_id: Option<ToolId>,
        canonical: &str,
        decl: Option<&ToolDecl>,
        content: Vec<ToolContent>,
        is_error: bool,
    ) -> Flow<Written> {
        let scrubber = self.shared.turns().scrubber();
        let mut texts: Vec<String> = Vec::new();
        let mut images: Vec<ResultContent> = Vec::new();
        let mut redactions = 0_u32;
        for part in content {
            match part {
                ToolContent::Text { text } => {
                    let (clean, hits) = scrubber.scrub(&text);
                    redactions += hits;
                    texts.push(clean.into_owned());
                }
                ToolContent::Image { media_type, data } => {
                    match blob::store(
                        &self.shared,
                        self.ctx.chat,
                        &media_type,
                        &data,
                        Source::Tool,
                    )
                    .await
                    {
                        Ok(blob) => images.push(ResultContent::Image { blob, media_type }),
                        Err(e) => texts.push(format!("[kernel] image dropped: {e}")),
                    }
                }
                ToolContent::Blob { id } => images.push(ResultContent::File { blob: id }),
                ToolContent::Unknown => {}
            }
        }
        if redactions > 0 {
            self.shared.bump(Metric::SecretsRedactions, canonical).await;
        }
        let full = texts.join("\n");
        let form = decl.map_or(ResultForm::Windowed, |d| d.result);
        let cfg = self.shared.cfg();
        let window = Window {
            form,
            max_tokens: cfg.tools.result.windowed_max_tokens,
            head_share: cfg.tools.result.head_share,
            reader_tokens: cfg.work.reader_result_tokens,
        };
        let model = self.model_id();
        let (chat, segment, turn, step, now) = self.coords();
        let input = u.input.clone();
        let call_id = u.id.clone();
        let canonical = canonical.to_owned();
        let ctx_cfg = cfg.context.clone();
        let written = self
            .shared
            .db()
            .write(move |tx| {
                let ratio = tx.turns().ratio(&model)?;
                let est = Estimator::new(ratio, &ctx_cfg);
                let tseq = tx.turns().next_tseq(chat)?;
                let contents = super::snapshot::encode(&(full.as_str(), &images));
                let key = dedup::dedup_key(tool_id, &canonical, &input, &contents);
                let dedup_of = tx.turns().find_dedup(chat, segment, &key)?;
                let mut shown = Vec::new();
                if dedup_of.is_none() {
                    if !full.is_empty() || images.is_empty() {
                        shown.push(ResultContent::text(window.apply(&full, tseq, &est)));
                    }
                    shown.extend(images.iter().cloned());
                }
                let payload = Payload::ToolResult(ToolResultPayload {
                    call_id: call_id.clone(),
                    tseq,
                    is_error,
                    content: shown.clone(),
                    dedup_of,
                    synthetic: None,
                });
                let at = Append {
                    chat,
                    segment,
                    turn_id: Some(&turn),
                    step: Some(step),
                    now,
                };
                let event = log::append(tx, at, &payload)?;
                tx.turns().insert_result(&NewToolResult {
                    chat,
                    tseq,
                    event,
                    segment,
                    tool_id,
                    dedup_key: &key,
                    dedup_of,
                    content: dedup_of.is_none().then_some(full.as_bytes()),
                    synthetic: None,
                    created_at: now,
                })?;
                DbResult::Ok(Written {
                    event,
                    tseq,
                    is_error,
                    content: shown,
                    synthetic: None,
                    dedup_of,
                })
            })
            .await?;
        self.slot.feed.note(written.event);
        Ok(written)
    }

    pub(super) fn feed_result(&self, u: &Use, written: &Written) {
        let status = match (written.synthetic, written.is_error) {
            (Some(Synthetic::Cancelled | Synthetic::NotStarted), _) => ItemStatus::Aborted,
            (_, true) => ItemStatus::Failed,
            (None, false) => ItemStatus::Completed,
            (Some(_), false) => ItemStatus::Failed,
        };
        let output = match written.dedup_of {
            Some(of) => vec![ToolContent::text(marker::same_as(of))],
            None => written
                .content
                .iter()
                .map(|c| match c {
                    ResultContent::Text { text } => ToolContent::text(text.clone()),
                    ResultContent::Image { blob, .. } | ResultContent::File { blob } => {
                        ToolContent::Blob { id: blob.clone() }
                    }
                })
                .collect(),
        };
        let item = feed::item(
            u.id.clone(),
            Some(&self.live.turn),
            written.event,
            status,
            ItemBody::ToolCall {
                call_id: u.id.clone(),
                name: u.name.clone(),
                input: u.input.clone(),
                agent: None,
                result: Some(ToolCallResult {
                    output,
                    is_error: written.is_error,
                }),
                progress: None,
            },
        );
        self.slot.feed.derived(
            written.event,
            0,
            FeedEvent::ItemCompleted {
                item,
                replaces: None,
            },
        );
    }

    fn apply(&mut self, effects: Effects) {
        for id in effects.loads.iter().collect::<BTreeSet<_>>() {
            self.tally().note_load(*id);
        }
        if let Some(effort) = effects.effort {
            self.effort = Some(effort);
        }
        self.compact_requested |= effects.compact;
        self.plan_changed |= effects.plan_changed;
    }

    fn coords(
        &self,
    ) -> (
        eva_protocol::types::ChatId,
        eva_protocol::types::SegmentId,
        eva_protocol::types::TurnId,
        u32,
        Timestamp,
    ) {
        (
            self.ctx.chat,
            self.segment,
            self.live.turn.clone(),
            self.step,
            self.shared.now(),
        )
    }
}

/// Записанный результат.
#[derive(Debug, Clone)]
pub struct Written {
    pub event: eva_protocol::types::EventId,
    pub tseq: Tseq,
    pub is_error: bool,
    pub content: Vec<ResultContent>,
    pub synthetic: Option<Synthetic>,
    pub dedup_of: Option<Tseq>,
}

/// Закрытие пары ядром внутри транзакции: `tool_result{synthetic}` и строка
/// `tool_results` вне дедупа (`01 §1.2`).
pub fn write_synthetic(
    tx: &Connection,
    at: Append<'_>,
    call_id: &str,
    tool_id: Option<ToolId>,
    kind: Synthetic,
) -> DbResult<Written> {
    let tseq = tx.turns().next_tseq(at.chat)?;
    let payload = ToolResultPayload::synthetic(call_id, tseq, kind);
    let content = payload.content.clone();
    let event = log::append(tx, at, &Payload::ToolResult(payload))?;
    let text = marker::synthetic(kind);
    let key = dedup::dedup_key(tool_id, call_id, &Value::Null, text.as_bytes());
    tx.turns().insert_result(&NewToolResult {
        chat: at.chat,
        tseq,
        event,
        segment: at.segment,
        tool_id,
        dedup_key: &key,
        dedup_of: None,
        content: Some(text.as_bytes()),
        synthetic: Some(kind.as_str()),
        created_at: at.now,
    })?;
    Ok(Written {
        event,
        tseq,
        is_error: true,
        content,
        synthetic: Some(kind),
        dedup_of: None,
    })
}

/// Окно результата (`02 §1.1`): `windowed` — голова, строка пропуска, хвост
/// по токенам; `uncapped` — хвост по `work.reader_result_tokens`.
#[derive(Debug, Clone, Copy)]
pub struct Window {
    pub form: ResultForm,
    pub max_tokens: u32,
    pub head_share: f32,
    pub reader_tokens: u32,
}

impl Window {
    pub fn apply(&self, text: &str, tseq: Tseq, est: &Estimator) -> String {
        match self.form {
            ResultForm::Uncapped => cut_tail(text, self.reader_tokens, est),
            ResultForm::Windowed => {
                if est.text(text.len()) <= self.max_tokens {
                    return text.to_owned();
                }
                let lines: Vec<&str> = text.lines().collect();
                let head_budget = (f64::from(self.max_tokens)
                    * f64::from(self.head_share.clamp(0.0, 1.0)))
                    as u32;
                let tail_budget = self.max_tokens.saturating_sub(head_budget);
                let mut head = 0;
                let mut used = 0_u32;
                while head < lines.len() {
                    let cost = est.text(lines[head].len() + 1);
                    if used + cost > head_budget {
                        break;
                    }
                    used += cost;
                    head += 1;
                }
                let mut tail = lines.len();
                let mut used = 0_u32;
                while tail > head {
                    let cost = est.text(lines[tail - 1].len() + 1);
                    if used + cost > tail_budget {
                        break;
                    }
                    used += cost;
                    tail -= 1;
                }
                if tail <= head {
                    return text.to_owned();
                }
                let omitted = (tail - head) as u64;
                let mut out: Vec<String> = lines[..head].iter().map(|l| (*l).to_owned()).collect();
                out.push(marker::lines_omitted(omitted, tseq, head as u64 + 1));
                out.extend(lines[tail..].iter().map(|l| (*l).to_owned()));
                out.join("\n")
            }
        }
    }
}

/// Хвост текста по токенам: последние строки, что влезают.
pub fn cut_tail(text: &str, budget: u32, est: &Estimator) -> String {
    if est.text(text.len()) <= budget {
        return text.to_owned();
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut start = lines.len();
    let mut used = 0_u32;
    while start > 0 {
        let cost = est.text(lines[start - 1].len() + 1);
        if used + cost > budget {
            break;
        }
        used += cost;
        start -= 1;
    }
    lines[start..].join("\n")
}
