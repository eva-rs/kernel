//! Рендер сегмента (`01 §2`): чистая функция от снимков сегмента и префикса
//! лога. Ни часов, ни `messages.*`, ни реестра — одинаковый вход даёт
//! одинаковые байты, префикс лога до точки запроса — префикс запроса (I1).
//!
//! Ответы модели (`thinking*`, `message{assistant}`, `tool_use*`) делят лог на
//! промежутки. Промежуток — одно `User` (сначала `tool_result` в порядке лога,
//! затем прочие блоки в порядке лога) и следом его `System` в порядке лога;
//! поэтому `System` не бывает первым, сразу после `Assistant` и перед `User`.
//! События, пришедшие во время инференса (между `upto` ответа и самим
//! ответом), рендерятся после ответа: его запрос их не видел.

use std::collections::BTreeSet;

use eva::data;
use jiff::tz::TimeZone;

use eva_protocol::types::{
    BlobId, Bytes, Effort, ModelId, Thinking,
    chat::Role,
    content::Block as ContentBlock,
    generate::{
        Block, CacheMode, ClearAt, GenerateRequest, Message, OutputConfig, Position, Purpose,
        ResultBlock, SystemKind, SystemSection, ToolChanges, ToolSpec,
    },
    model::ModelCaps,
};

use crate::subsystems::turns::{
    log::{
        Event, MessagePayload, Payload, ResultContent, ThinkingBlob, ThinkingPayload,
        ToolResultPayload, repair_tool_pairs,
    },
    marker,
    snapshot::SegmentSnapshot,
};

/// Blob-store для рендера; отсутствие — маркер `[image <id>]` / `[file <id>]`.
pub trait Blobs {
    fn meta(&self, id: &BlobId) -> Option<BlobMeta>;
    fn bytes(&self, id: &BlobId) -> Option<Bytes>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobMeta {
    pub name: String,
    pub size: u64,
}

/// Blob-store пуст: каждое вложение — маркер.
pub struct NoBlobs;

impl Blobs for NoBlobs {
    fn meta(&self, _: &BlobId) -> Option<BlobMeta> {
        None
    }

    fn bytes(&self, _: &BlobId) -> Option<Bytes> {
        None
    }
}

pub struct RenderInput<'a> {
    pub segment: &'a SegmentSnapshot,
    /// Лог сегмента до точки запроса включительно.
    pub events: &'a [Event],
    /// Capabilities модели шага: выбор fallback (`01 §3`).
    pub caps: &'a ModelCaps,
    pub blobs: &'a dyn Blobs,
    /// `system.tz`: время маркера спикера.
    pub tz: &'a TimeZone,
}

/// Нормализованный запрос без параметров шага.
#[data]
pub struct Rendered {
    pub system: Vec<SystemSection>,
    pub tools: Vec<ToolSpec>,
    pub messages: Vec<Message>,
}

/// Параметры шага поверх рендера (`01 §2.1`, `§7`, `§8`).
#[derive(Debug, Clone)]
pub struct RequestParams {
    pub model: ModelId,
    pub effort: Effort,
    pub thinking: Thinking,
    pub breakpoints: Vec<Position>,
    pub cache: CacheMode,
    pub max_tokens: u32,
    pub purpose: Purpose,
}

impl Rendered {
    /// Канонические байты: сравнение префиксов и тесты детерминизма.
    pub fn bytes(&self) -> Vec<u8> {
        rmp_serde::to_vec_named(self).unwrap_or_default()
    }

    /// `effort` top-level, только если модель не несёт его в `messages` (`01 §3`).
    pub fn into_request(self, params: RequestParams, caps: &ModelCaps) -> GenerateRequest {
        GenerateRequest {
            model: params.model,
            system: self.system,
            tools: self.tools,
            messages: self.messages,
            breakpoints: params.breakpoints,
            cache: params.cache,
            effort: (!Forms::of(caps).effort).then_some(params.effort),
            thinking: Some(params.thinking),
            max_tokens: params.max_tokens,
            format: None,
            purpose: params.purpose,
        }
    }
}

/// Формы, которые модель объявила; формы в `messages` требуют `system_in_messages`.
#[derive(Debug, Clone, Copy)]
struct Forms {
    vision: bool,
    system: bool,
    tool_changes: bool,
    clear_at: bool,
    effort: bool,
    thinking_binding: bool,
}

impl Forms {
    fn of(caps: &ModelCaps) -> Self {
        let system = caps.system_in_messages;
        Self {
            vision: caps.vision,
            system,
            tool_changes: system && caps.tool_changes,
            clear_at: system && caps.clear_at,
            effort: system && caps.effort_in_messages,
            thinking_binding: caps.thinking_binding,
        }
    }
}

/// `tools[]` запроса (`01 §2.3` п.2): с `tool_changes` — снимок целиком, без —
/// не-deferred записи снимка и `tools_added` хвостом. Exposure шага его не трогает (I7).
pub fn tools(segment: &SegmentSnapshot, events: &[Event], caps: &ModelCaps) -> Vec<ToolSpec> {
    if Forms::of(caps).tool_changes {
        return segment.tools.clone();
    }
    let mut out: Vec<ToolSpec> = segment
        .tools
        .iter()
        .filter(|t| !t.deferred)
        .cloned()
        .collect();
    for event in events {
        if let Payload::ToolsAdded(added) = &event.payload {
            for spec in &added.tools {
                if !out.iter().any(|t| t.name == spec.name) {
                    out.push(ToolSpec {
                        deferred: false,
                        ..spec.clone()
                    });
                }
            }
        }
    }
    out
}

pub fn render(input: RenderInput<'_>) -> Rendered {
    let forms = Forms::of(input.caps);
    let repaired = repair_tool_pairs(input.events);
    if repaired.fixes > 0 {
        log::warn!(fixes = repaired.fixes; "segment log needed tool pair repair");
    }
    debug_assert_eq!(repaired.fixes, 0, "rendered a log with open tool pairs");

    let mut b = Builder::new(forms, input.blobs, input.tz);
    for event in order(&repaired.events, forms) {
        b.event(event);
    }
    Rendered {
        system: input.segment.system.clone(),
        tools: tools(input.segment, input.events, input.caps),
        messages: b.finish(),
    }
}

/// Порядок рендера: события между `upto` ответа и ответом — сразу после его
/// `tool_use`; под `thinking_binding` без `tool_changes` thinking до последней
/// дозаписи tools не реплеится (подпись привязана к набору tools, `01 §3`).
fn order(events: &[Event], forms: Forms) -> Vec<&Event> {
    let cut = (forms.thinking_binding && !forms.tool_changes)
        .then(|| {
            events
                .iter()
                .rposition(|e| matches!(e.payload, Payload::ToolsAdded(_)))
        })
        .flatten();
    let mut out: Vec<&Event> = Vec::with_capacity(events.len());
    let mut held: Vec<&Event> = Vec::new();
    let mut in_response = false;
    for (i, event) in events.iter().enumerate() {
        match &event.payload {
            Payload::Thinking(_) if cut.is_some_and(|c| i < c) => {}
            Payload::Message(m) if m.role == Role::Assistant => {
                out.append(&mut held);
                let own = out
                    .iter()
                    .rev()
                    .take_while(|e| matches!(e.payload, Payload::Thinking(_)))
                    .count();
                let own = out.split_off(out.len() - own);
                if let Some(upto) = m.upto {
                    let late = out.iter().rev().take_while(|e| e.id > upto).count();
                    held = out.split_off(out.len() - late);
                }
                out.extend(own);
                out.push(event);
                in_response = true;
            }
            Payload::ToolUse(_) if in_response => out.push(event),
            _ => {
                in_response = false;
                out.append(&mut held);
                out.push(event);
            }
        }
    }
    out.append(&mut held);
    out
}

fn text(s: &str) -> Block {
    Block::Text { text: s.to_owned() }
}

fn system(kind: SystemKind, blocks: Vec<Block>) -> Message {
    Message::System {
        kind,
        blocks,
        clear_at: None,
        output_config: None,
        tool_changes: None,
    }
}

/// Промежуток между ответами модели.
#[derive(Default)]
struct Gap {
    results: Vec<Block>,
    rest: Vec<Block>,
    /// Нативная форма и её fallback-блоки: промежуток без user-части сворачивает `System` в текст.
    systems: Vec<(Message, Vec<Block>)>,
}

/// Ответ модели в сборке.
#[derive(Default)]
struct Response {
    thinking: Vec<Block>,
    body: Vec<Block>,
    uses: Vec<Block>,
    /// `message{assistant}` уже пришёл: следующий thinking или message — новый ответ.
    answered: bool,
}

struct Builder<'a> {
    forms: Forms,
    blobs: &'a dyn Blobs,
    tz: &'a TimeZone,
    out: Vec<Message>,
    gap: Gap,
    response: Option<Response>,
    /// Календарные сутки, уже получившие маркер с датой.
    dated: BTreeSet<jiff::civil::Date>,
    /// `effort` последнего `params_changed`: нативная форма — только при смене.
    effort: Option<Effort>,
}

impl<'a> Builder<'a> {
    fn new(forms: Forms, blobs: &'a dyn Blobs, tz: &'a TimeZone) -> Self {
        Self {
            forms,
            blobs,
            tz,
            out: Vec::new(),
            gap: Gap::default(),
            response: None,
            dated: BTreeSet::new(),
            effort: None,
        }
    }

    fn finish(mut self) -> Vec<Message> {
        self.close_response();
        self.flush_gap();
        self.out
    }

    fn event(&mut self, event: &Event) {
        match &event.payload {
            Payload::Thinking(t) => {
                if let Some(block) = thinking(t) {
                    self.response(true).thinking.push(block);
                }
            }
            Payload::Message(m) if m.role == Role::Assistant => {
                let blocks: Vec<Block> = m.blocks.iter().map(|b| self.said(b)).collect();
                let response = self.response(true);
                response.body.extend(blocks);
                response.answered = true;
            }
            Payload::ToolUse(u) => self.response(false).uses.push(Block::ToolUse {
                id: u.call_id.clone(),
                name: u.name.clone(),
                input: u.input.clone(),
            }),
            Payload::Message(m) => {
                if m.consumed_by.is_none() {
                    let blocks = self.speech(event, m);
                    self.user(blocks);
                }
            }
            Payload::ToolResult(r) => {
                let block = Block::ToolResult {
                    tool_use_id: r.call_id.clone(),
                    content: self.result(r),
                    is_error: r.is_error,
                };
                self.close_response();
                self.gap.results.push(block);
            }
            // Всегда user-роль: после refusal/фильтра — перед следующей репликой, после частичного ответа — продолжение.
            Payload::SystemText(s) => self.user(vec![text(&s.text)]),
            Payload::NoteUpdated(n) => {
                self.hygiene(SystemKind::NoteUpdated, marker::note_updated(n));
            }
            Payload::MemoryUpdated(m) => {
                self.hygiene(SystemKind::MemoryUpdated, marker::memory_updated(m));
            }
            Payload::Reminder(r) => {
                let fallback = text(&marker::reminder_fallback(&r.text));
                if self.forms.clear_at {
                    let native = Message::System {
                        kind: SystemKind::Reminder,
                        blocks: vec![text(&r.text)],
                        clear_at: Some(ClearAt::NextUserMessage),
                        output_config: None,
                        tool_changes: None,
                    };
                    self.system(native, vec![fallback]);
                } else {
                    self.user(vec![fallback]);
                }
            }
            Payload::ParamsChanged(p) => {
                let changed = self.effort.replace(p.effort) != Some(p.effort);
                if self.forms.effort && changed {
                    let native = Message::System {
                        kind: SystemKind::ParamsChanged,
                        blocks: Vec::new(),
                        clear_at: None,
                        output_config: Some(OutputConfig { effort: p.effort }),
                        tool_changes: None,
                    };
                    self.system(native, Vec::new());
                }
            }
            Payload::ToolsAdded(a) => {
                if self.forms.tool_changes {
                    let native = Message::System {
                        kind: SystemKind::ToolsAdded,
                        blocks: Vec::new(),
                        clear_at: None,
                        output_config: None,
                        tool_changes: Some(ToolChanges {
                            added: a.tools.iter().map(|t| t.name.clone()).collect(),
                        }),
                    };
                    self.system(native, Vec::new());
                }
            }
            Payload::ToolsRemoved(_) | Payload::SegmentReset(_) | Payload::TurnEnded(_) => {}
        }
    }

    /// Открытый ответ; `starts` — событие может начать новый ответ, если у открытого уже есть message.
    fn response(&mut self, starts: bool) -> &mut Response {
        if self.response.as_ref().is_some_and(|r| starts && r.answered) {
            self.close_response();
        }
        self.response.get_or_insert_with(Response::default)
    }

    /// Пустой ответ (thinking отрезан, текста нет) границей промежутков не служит.
    fn close_response(&mut self) {
        let Some(r) = self.response.take() else {
            return;
        };
        let mut blocks = r.thinking;
        blocks.extend(r.body);
        blocks.extend(r.uses);
        if blocks.is_empty() {
            return;
        }
        self.flush_gap();
        self.out.push(Message::Assistant { blocks });
    }

    fn flush_gap(&mut self) {
        let gap = std::mem::take(&mut self.gap);
        let mut blocks = gap.results;
        blocks.extend(gap.rest);
        if blocks.is_empty() {
            // Запроса без user-части не бывает; промежуток в хвосте простаивающего чата — текстом.
            let fallback: Vec<Block> = gap.systems.into_iter().flat_map(|(_, f)| f).collect();
            if !fallback.is_empty() {
                self.out.push(Message::User { blocks: fallback });
            }
            return;
        }
        self.out.push(Message::User { blocks });
        self.out.extend(gap.systems.into_iter().map(|(m, _)| m));
    }

    fn user(&mut self, blocks: Vec<Block>) {
        self.close_response();
        self.gap.rest.extend(blocks);
    }

    fn system(&mut self, native: Message, fallback: Vec<Block>) {
        self.close_response();
        self.gap.systems.push((native, fallback));
    }

    /// Событие гигиены: нативно `System`, иначе текст с маркером в ещё не отправленном `User`.
    fn hygiene(&mut self, kind: SystemKind, body: String) {
        if self.forms.system {
            self.system(system(kind, vec![text(&body)]), vec![text(&body)]);
        } else {
            self.user(vec![text(&body)]);
        }
    }

    /// Реплика: маркер спикера из payload и тело под `| `; текст ядра — как есть.
    fn speech(&mut self, event: &Event, m: &MessagePayload) -> Vec<Block> {
        let Some(speaker) = &m.speaker else {
            return m.blocks.iter().map(|b| self.content(b)).collect();
        };
        let first_of_day = self.dated.insert(marker::day(event.created_at, self.tz));
        let line = marker::speaker_line(
            speaker,
            m.reply.as_ref(),
            m.ext_id.as_deref(),
            event.created_at,
            self.tz,
            first_of_day,
        );
        let body: Vec<&str> = m
            .blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let mut out = vec![text(&marker::speech(&line, &body.join("\n")))];
        out.extend(
            m.blocks
                .iter()
                .filter(|b| !matches!(b, ContentBlock::Text { .. }))
                .map(|b| self.content(b)),
        );
        out
    }

    /// Блок реплики: картинка байтами для `vision`, иначе маркер; файл — всегда маркер.
    fn content(&self, block: &ContentBlock) -> Block {
        match block {
            ContentBlock::Text { text } => Block::Text { text: text.clone() },
            ContentBlock::Image { blob, media_type } => match self.image(blob) {
                Some(data) => Block::Image {
                    media_type: media_type.clone(),
                    data,
                },
                None => text(&self.image_marker(blob)),
            },
            ContentBlock::File { blob } => text(&self.file_marker(blob)),
        }
    }

    /// Блок ответа модели: картинки в assistant-роли не реплеятся — маркер.
    fn said(&self, block: &ContentBlock) -> Block {
        match block {
            ContentBlock::Text { text } => Block::Text { text: text.clone() },
            ContentBlock::Image { blob, .. } => text(&self.image_marker(blob)),
            ContentBlock::File { blob } => text(&self.file_marker(blob)),
        }
    }

    fn image(&self, blob: &BlobId) -> Option<Bytes> {
        self.forms.vision.then(|| self.blobs.bytes(blob)).flatten()
    }

    fn image_marker(&self, blob: &BlobId) -> String {
        marker::image(
            blob,
            self.blobs.meta(blob).as_ref().map(|m| m.name.as_str()),
        )
    }

    fn file_marker(&self, blob: &BlobId) -> String {
        let meta = self.blobs.meta(blob);
        marker::file(blob, meta.as_ref().map(|m| (m.name.as_str(), m.size)))
    }

    /// `[t<seq>]` первой строкой, затем контент либо ссылка дедупа (`01 §4`).
    fn result(&self, r: &ToolResultPayload) -> Vec<ResultBlock> {
        let mut out = Vec::new();
        let mut lines: Vec<String> = Vec::new();
        if r.tseq.get() > 0 {
            lines.push(marker::tool_header(r.tseq));
        }
        let flush = |lines: &mut Vec<String>, out: &mut Vec<ResultBlock>| {
            if !lines.is_empty() {
                out.push(ResultBlock::Text {
                    text: lines.join("\n"),
                });
                lines.clear();
            }
        };
        match r.dedup_of {
            Some(of) => lines.push(marker::same_as(of)),
            None => {
                for part in &r.content {
                    match part {
                        ResultContent::Text { text } => lines.push(text.clone()),
                        ResultContent::File { blob } => lines.push(self.file_marker(blob)),
                        ResultContent::Image { blob, media_type } => match self.image(blob) {
                            Some(data) => {
                                flush(&mut lines, &mut out);
                                out.push(ResultBlock::Image {
                                    media_type: media_type.clone(),
                                    data,
                                });
                            }
                            None => lines.push(self.image_marker(blob)),
                        },
                    }
                }
            }
        }
        flush(&mut lines, &mut out);
        if out.is_empty() {
            out.push(ResultBlock::Text {
                text: String::new(),
            });
        }
        out
    }
}

/// Блок thinking для реплея; без подписи не реплеится (`01 §1.2`).
fn thinking(t: &ThinkingPayload) -> Option<Block> {
    match &t.blob {
        ThinkingBlob::Signed { signature, .. } if signature.is_empty() => None,
        ThinkingBlob::Signed { signature, text } => Some(Block::Thinking {
            signature: signature.clone(),
            model: t.model.clone(),
            text: text.clone(),
        }),
        ThinkingBlob::Redacted { data } => Some(Block::RedactedThinking {
            data: data.clone(),
            model: t.model.clone(),
        }),
    }
}

/// Первое сообщение сегмента после компакции (`01 §2.3` п.3, `§6.2` п.6): summary
/// и хвост под `| `, строка переноса заметок; пишется событием
/// `message{user, speaker: None}` — рендер читает только лог.
pub fn compaction_opener(
    summary: &str,
    verbatim_tail: Option<&str>,
    notes_carried: u32,
) -> Vec<ContentBlock> {
    let mut out = format!("{}\n{}", marker::COMPACTION_SUMMARY, marker::quote(summary));
    if let Some(tail) = verbatim_tail.filter(|t| !t.is_empty()) {
        out.push('\n');
        out.push_str(marker::VERBATIM_TAIL);
        out.push('\n');
        out.push_str(&marker::quote(tail));
    }
    if notes_carried > 0 {
        out.push('\n');
        out.push_str(&marker::notes_carried_over(notes_carried));
    }
    vec![ContentBlock::text(out)]
}
