//! Лог сегмента (`01 §1.2`): payload'ы событий, строка БД → `Event`,
//! append в транзакции писателя и страховка `repair_tool_pairs`.
//!
//! Payload хранится msgpack-ом (`to_vec_named`) под `events.kind`; незнакомый
//! `kind` или нечитаемый payload — `Aux::Internal`, не молчаливый пропуск (I1).
//! Ответ шага (`thinking`, `message{assistant}`, `tool_use`) пишется одной
//! транзакцией: рендер опирается на его непрерывность.

use eva::{data, time::Timestamp};
use serde_json::Value;

use eva_protocol::{
    errors::Aux,
    types::turn::TurnOutcome,
    types::{
        BlobId, Bytes, ChatId, Effort, EventId, MemoryId, ModelId, ModuleId, PersonId, SegmentId,
        Thinking, ToolId, Tseq, TurnId,
        chat::{Role, SegmentReason},
        content::Block,
        generate::{StopReason, ToolSpec},
        note::NoteUpdated,
    },
};

use crate::{
    components::Components,
    db::{
        Connection, Db as _, DbResult, Queries as _,
        turns::{EventRow, NewEvent, SegmentRow},
    },
    shared::Shared,
    subsystems::turns::{
        marker::{self, SystemText},
        snapshot::SegmentSnapshot,
    },
};

/// `events.kind`.
#[data(copy, ord, display(name))]
#[derive(Hash)]
pub enum Kind {
    Message,
    Thinking,
    ToolUse,
    ToolResult,
    SystemText,
    NoteUpdated,
    MemoryUpdated,
    ToolsAdded,
    ToolsRemoved,
    Reminder,
    ParamsChanged,
    SegmentReset,
    TurnEnded,
}

impl Kind {
    pub const ALL: [Self; 13] = [
        Self::Message,
        Self::Thinking,
        Self::ToolUse,
        Self::ToolResult,
        Self::SystemText,
        Self::NoteUpdated,
        Self::MemoryUpdated,
        Self::ToolsAdded,
        Self::ToolsRemoved,
        Self::Reminder,
        Self::ParamsChanged,
        Self::SegmentReset,
        Self::TurnEnded,
    ];

    /// Имя под CHECK `events.kind`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Thinking => "thinking",
            Self::ToolUse => "tool_use",
            Self::ToolResult => "tool_result",
            Self::SystemText => "system_text",
            Self::NoteUpdated => "note_updated",
            Self::MemoryUpdated => "memory_updated",
            Self::ToolsAdded => "tools_added",
            Self::ToolsRemoved => "tools_removed",
            Self::Reminder => "reminder",
            Self::ParamsChanged => "params_changed",
            Self::SegmentReset => "segment_reset",
            Self::TurnEnded => "turn_ended",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// Снимок спикера на момент append; `name`/`handle` уже через `sanitize_inline`
/// и обрезку `chats.marker.name_tokens`: рендер без ratio (I1).
#[data]
pub struct Speaker {
    pub person_id: Option<PersonId>,
    pub kind: SpeakerKind,
    pub name: Option<String>,
    pub handle: Option<String>,
}

#[data(copy, ord, display(name))]
pub enum SpeakerKind {
    Master,
    User,
    Unknown,
}

/// Цель ответа; `snippet` уже через `sanitize_inline`, только если цель вне сегмента.
#[data]
pub struct Reply {
    pub ext_id: String,
    /// Цель — реплика Евы.
    pub yours: bool,
    pub snippet: Option<String>,
}

#[data]
pub struct MessagePayload {
    pub role: Role,
    /// `None` — текст ядра (`p0`): первая строка блока — маркер реестра.
    pub speaker: Option<Speaker>,
    pub reply: Option<Reply>,
    pub blocks: Vec<Block>,
    pub ext_id: Option<String>,
    /// `text_instead` для `chat_ask`: тело живёт внутри `tool_result` этого вызова.
    pub consumed_by: Option<String>,
    pub model: Option<ModelId>,
    pub stop: Option<StopReason>,
    /// Ответ оборван (`01 §10`).
    #[serde(default)]
    pub partial: bool,
    /// Assistant: последнее событие запроса, породившего ответ. События между
    /// ним и ответом пришли во время инференса и рендерятся после ответа (`01 §2.3`).
    pub upto: Option<EventId>,
}

impl MessagePayload {
    /// Реплика человека с маркером спикера.
    pub fn user(speaker: Speaker, blocks: Vec<Block>) -> Self {
        Self {
            speaker: Some(speaker),
            ..Self::kernel(blocks)
        }
    }

    /// Текст ядра в user-роли: блоки уже начинаются маркером реестра.
    pub fn kernel(blocks: Vec<Block>) -> Self {
        Self {
            role: Role::User,
            speaker: None,
            reply: None,
            blocks,
            ext_id: None,
            consumed_by: None,
            model: None,
            stop: None,
            partial: false,
            upto: None,
        }
    }

    /// Ответ модели на запрос, видевший лог до `upto` включительно.
    pub fn assistant(
        model: ModelId,
        blocks: Vec<Block>,
        stop: Option<StopReason>,
        partial: bool,
        upto: EventId,
    ) -> Self {
        Self {
            role: Role::Assistant,
            model: Some(model),
            stop,
            partial,
            upto: Some(upto),
            ..Self::kernel(blocks)
        }
    }
}

#[data]
pub struct ThinkingPayload {
    pub model: ModelId,
    pub blob: ThinkingBlob,
}

/// Непрозрачный блок размышления; `Signed` с пустой подписью не реплеится.
#[data]
pub enum ThinkingBlob {
    Signed {
        signature: Bytes,
        text: Option<String>,
    },
    Redacted {
        data: Bytes,
    },
}

#[data]
pub struct ToolUsePayload {
    pub call_id: String,
    /// `None` — `extra_tools` тура.
    pub tool_id: Option<ToolId>,
    /// Каноничное имя на момент шага.
    pub name: String,
    pub input: Value,
}

/// Закрытие висячего `tool_use` ядром.
#[data(copy, ord, display(name))]
pub enum Synthetic {
    NotStarted,
    Interrupted,
    Cancelled,
    ToolUnavailable,
}

impl Synthetic {
    /// Имя под CHECK `tool_results.synthetic`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Interrupted => "interrupted",
            Self::Cancelled => "cancelled",
            Self::ToolUnavailable => "tool_unavailable",
        }
    }
}

/// Что видит модель: окно уже применено, байты картинок — в blob-store.
#[data]
pub enum ResultContent {
    Text { text: String },
    Image { blob: BlobId, media_type: String },
    File { blob: BlobId },
}

impl ResultContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

#[data]
pub struct ToolResultPayload {
    pub call_id: String,
    /// `t0` — результат не из БД (`repair_tool_pairs`), заголовка `[t<seq>]` нет.
    pub tseq: Tseq,
    pub is_error: bool,
    pub content: Vec<ResultContent>,
    pub dedup_of: Option<Tseq>,
    pub synthetic: Option<Synthetic>,
}

impl ToolResultPayload {
    /// Закрытие пары ядром: `is_error`, текст — строка реестра, вне дедупа.
    pub fn synthetic(call_id: impl Into<String>, tseq: Tseq, kind: Synthetic) -> Self {
        Self {
            call_id: call_id.into(),
            tseq,
            is_error: true,
            content: vec![ResultContent::text(marker::synthetic(kind))],
            dedup_of: None,
            synthetic: Some(kind),
        }
    }
}

/// Текст снят при append: рендер читает только payload (I1).
#[data]
pub struct SystemTextPayload {
    pub marker: SystemText,
    pub text: String,
}

impl SystemTextPayload {
    pub fn new(marker: SystemText) -> Self {
        let text = marker.render();
        Self { marker, text }
    }
}

#[data]
pub struct MemoryUpdatedPayload {
    /// Строки формата `03 §5`, возраст зафиксирован на момент события.
    pub added: Vec<String>,
    pub removed: Vec<MemoryId>,
}

#[data]
pub struct ToolsAddedPayload {
    pub tools: Vec<ToolSpec>,
}

#[data]
pub struct ToolsRemovedPayload {
    pub names: Vec<String>,
}

/// `clear_at = next_user_message` подразумевается.
#[data]
pub struct ReminderPayload {
    pub text: String,
}

/// Эффективные параметры шага, отличные от предыдущего шага сегмента (первый шаг — всегда).
#[data]
pub struct ParamsChangedPayload {
    pub model: ModelId,
    pub effort: Effort,
    pub thinking: Thinking,
}

#[data]
pub struct SegmentResetPayload {
    pub new_segment: SegmentId,
    pub reason: SegmentReason,
}

#[data]
pub struct TurnEndedPayload {
    pub turn_id: TurnId,
    pub outcome: TurnOutcome,
    pub by: Option<EndedBy>,
    pub error: Option<String>,
}

/// Кто закрыл тур (`01 §1.2`, `07 §8`).
#[data]
pub enum EndedBy {
    Tool {
        tool_id: Option<ToolId>,
        canonical: String,
    },
    Client {
        module: ModuleId,
    },
    Lead,
    ChatDeleted,
    Shutdown,
    Gc,
    StepLimit,
    Hook {
        module: ModuleId,
    },
}

/// Типизированный payload события.
#[data]
pub enum Payload {
    Message(MessagePayload),
    Thinking(ThinkingPayload),
    ToolUse(ToolUsePayload),
    ToolResult(ToolResultPayload),
    SystemText(SystemTextPayload),
    NoteUpdated(NoteUpdated),
    MemoryUpdated(MemoryUpdatedPayload),
    ToolsAdded(ToolsAddedPayload),
    ToolsRemoved(ToolsRemovedPayload),
    Reminder(ReminderPayload),
    ParamsChanged(ParamsChangedPayload),
    SegmentReset(SegmentResetPayload),
    TurnEnded(TurnEndedPayload),
}

/// Лог не читается: база испорчена или схема новее сборки.
#[data(error)]
pub enum LogError {
    #[display("event {id}: unknown kind {kind:?}")]
    UnknownKind { id: EventId, kind: String },
    #[display("event {id} ({kind}): payload does not decode: {error}")]
    Payload {
        id: EventId,
        kind: Kind,
        error: String,
    },
    #[display("segment {segment}: snapshot does not decode")]
    Snapshot { segment: SegmentId },
}

impl From<LogError> for Aux {
    fn from(e: LogError) -> Self {
        Self::Internal(e.to_string())
    }
}

impl Payload {
    pub const fn kind(&self) -> Kind {
        match self {
            Self::Message(_) => Kind::Message,
            Self::Thinking(_) => Kind::Thinking,
            Self::ToolUse(_) => Kind::ToolUse,
            Self::ToolResult(_) => Kind::ToolResult,
            Self::SystemText(_) => Kind::SystemText,
            Self::NoteUpdated(_) => Kind::NoteUpdated,
            Self::MemoryUpdated(_) => Kind::MemoryUpdated,
            Self::ToolsAdded(_) => Kind::ToolsAdded,
            Self::ToolsRemoved(_) => Kind::ToolsRemoved,
            Self::Reminder(_) => Kind::Reminder,
            Self::ParamsChanged(_) => Kind::ParamsChanged,
            Self::SegmentReset(_) => Kind::SegmentReset,
            Self::TurnEnded(_) => Kind::TurnEnded,
        }
    }

    /// Байты `events.payload`.
    pub fn encode(&self) -> Vec<u8> {
        let encoded = match self {
            Self::Message(p) => rmp_serde::to_vec_named(p),
            Self::Thinking(p) => rmp_serde::to_vec_named(p),
            Self::ToolUse(p) => rmp_serde::to_vec_named(p),
            Self::ToolResult(p) => rmp_serde::to_vec_named(p),
            Self::SystemText(p) => rmp_serde::to_vec_named(p),
            Self::NoteUpdated(p) => rmp_serde::to_vec_named(p),
            Self::MemoryUpdated(p) => rmp_serde::to_vec_named(p),
            Self::ToolsAdded(p) => rmp_serde::to_vec_named(p),
            Self::ToolsRemoved(p) => rmp_serde::to_vec_named(p),
            Self::Reminder(p) => rmp_serde::to_vec_named(p),
            Self::ParamsChanged(p) => rmp_serde::to_vec_named(p),
            Self::SegmentReset(p) => rmp_serde::to_vec_named(p),
            Self::TurnEnded(p) => rmp_serde::to_vec_named(p),
        };
        // Свои `#[data]` без карт с нестроковыми ключами: сериализация не падает.
        encoded.unwrap_or_default()
    }

    pub fn decode(id: EventId, kind: Kind, bytes: &[u8]) -> Result<Self, LogError> {
        fn read<T: serde::de::DeserializeOwned>(
            id: EventId,
            kind: Kind,
            bytes: &[u8],
        ) -> Result<T, LogError> {
            rmp_serde::from_slice(bytes).map_err(|e| LogError::Payload {
                id,
                kind,
                error: e.to_string(),
            })
        }
        Ok(match kind {
            Kind::Message => Self::Message(read(id, kind, bytes)?),
            Kind::Thinking => Self::Thinking(read(id, kind, bytes)?),
            Kind::ToolUse => Self::ToolUse(read(id, kind, bytes)?),
            Kind::ToolResult => Self::ToolResult(read(id, kind, bytes)?),
            Kind::SystemText => Self::SystemText(read(id, kind, bytes)?),
            Kind::NoteUpdated => Self::NoteUpdated(read(id, kind, bytes)?),
            Kind::MemoryUpdated => Self::MemoryUpdated(read(id, kind, bytes)?),
            Kind::ToolsAdded => Self::ToolsAdded(read(id, kind, bytes)?),
            Kind::ToolsRemoved => Self::ToolsRemoved(read(id, kind, bytes)?),
            Kind::Reminder => Self::Reminder(read(id, kind, bytes)?),
            Kind::ParamsChanged => Self::ParamsChanged(read(id, kind, bytes)?),
            Kind::SegmentReset => Self::SegmentReset(read(id, kind, bytes)?),
            Kind::TurnEnded => Self::TurnEnded(read(id, kind, bytes)?),
        })
    }
}

/// Событие лога с разобранным payload.
#[data]
pub struct Event {
    pub id: EventId,
    pub chat: ChatId,
    pub segment: SegmentId,
    pub seq: u32,
    pub turn_id: Option<TurnId>,
    /// Номер шага у событий движка; `None` у ingest и гигиены.
    pub step: Option<u32>,
    pub created_at: Timestamp,
    pub payload: Payload,
}

impl TryFrom<EventRow> for Event {
    type Error = LogError;

    fn try_from(row: EventRow) -> Result<Self, LogError> {
        let kind = Kind::parse(&row.kind).ok_or_else(|| LogError::UnknownKind {
            id: row.id,
            kind: row.kind.clone(),
        })?;
        Ok(Self {
            id: row.id,
            chat: row.chat,
            segment: row.segment,
            seq: row.seq,
            turn_id: row.turn_id,
            step: row.step,
            created_at: row.created_at,
            payload: Payload::decode(row.id, kind, &row.payload)?,
        })
    }
}

/// Строки сегмента → события; первая нечитаемая — ошибка.
pub fn decode_all(rows: Vec<EventRow>) -> Result<Vec<Event>, LogError> {
    rows.into_iter().map(Event::try_from).collect()
}

/// Событие к append; `step` — у событий движка.
#[derive(Debug, Clone, Copy)]
pub struct Append<'a> {
    pub chat: ChatId,
    pub segment: SegmentId,
    pub turn_id: Option<&'a TurnId>,
    pub step: Option<u32>,
    pub now: Timestamp,
}

/// Append в лог сегмента на читателе-писателе вызывающего (внутри транзакции).
pub fn append(conn: &Connection, at: Append<'_>, payload: &Payload) -> DbResult<EventId> {
    conn.turns().append(&NewEvent {
        chat: at.chat,
        segment: at.segment,
        kind: payload.kind().as_str(),
        payload: &payload.encode(),
        turn_id: at.turn_id,
        step: at.step,
        created_at: at.now,
    })
}

/// Событие гигиены от другой подсистемы.
#[data]
pub enum HygienePayload {
    NoteUpdated(NoteUpdated),
    MemoryUpdated(MemoryUpdatedPayload),
}

/// Единственный публичный append для других подсистем: в открытый сегмент
/// чата, `step = NULL`. Без открытого сегмента ничего не пишется — его первый
/// снимок system соберёт секции из БД (`04 §8`).
pub async fn append_hygiene<C: Components>(
    shared: &Shared<C>,
    chat: ChatId,
    payload: HygienePayload,
) -> DbResult<Option<EventId>> {
    let now = shared.now();
    let payload = match payload {
        HygienePayload::NoteUpdated(p) => Payload::NoteUpdated(p),
        HygienePayload::MemoryUpdated(p) => Payload::MemoryUpdated(p),
    };
    shared
        .db()
        .write(move |tx| {
            let Some(segment) = tx.turns().open_segment(chat)? else {
                return Ok(None);
            };
            let at = Append {
                chat,
                segment: segment.id,
                turn_id: None,
                step: None,
                now,
            };
            append(tx, at, &payload).map(Some)
        })
        .await
}

/// Страховка рендера (`01 §1.2`): результаты без своего `tool_use` в том же
/// раунде выбрасываются, висячие вызовы получают `synthetic: interrupted` в
/// конце раунда (следующий ответ, граница шага, конец тура или сегмента).
/// На корректном логе — no-op; идемпотентна.
pub fn repair_tool_pairs(events: &[Event]) -> Repaired {
    let mut out = Vec::with_capacity(events.len());
    let mut fixes = 0_u32;
    // Открытые вызовы раунда и позиция их `tool_use` в `out`: синтетика наследует адрес события.
    let mut open: Vec<(String, usize)> = Vec::new();

    for event in events {
        let starts_round = match &event.payload {
            Payload::Thinking(_)
            | Payload::ParamsChanged(_)
            | Payload::Reminder(_)
            | Payload::TurnEnded(_)
            | Payload::SegmentReset(_) => true,
            Payload::Message(m) => m.role == Role::Assistant,
            _ => false,
        };
        if starts_round {
            close_round(&mut open, &mut out, &mut fixes);
        }
        match &event.payload {
            Payload::ToolUse(u) => {
                open.push((u.call_id.clone(), out.len()));
                out.push(event.clone());
            }
            Payload::ToolResult(r) => match open.iter().position(|(id, _)| *id == r.call_id) {
                Some(pos) => {
                    open.remove(pos);
                    out.push(event.clone());
                }
                None => fixes += 1,
            },
            _ => out.push(event.clone()),
        }
    }
    close_round(&mut open, &mut out, &mut fixes);
    Repaired { events: out, fixes }
}

fn close_round(open: &mut Vec<(String, usize)>, out: &mut Vec<Event>, fixes: &mut u32) {
    for (call_id, at) in open.drain(..) {
        let payload = Payload::ToolResult(ToolResultPayload::synthetic(
            call_id,
            Tseq(0),
            Synthetic::Interrupted,
        ));
        let synthetic = Event {
            payload,
            ..out[at].clone()
        };
        out.push(synthetic);
        *fixes += 1;
    }
}

/// Лог после `repair_tool_pairs`; `fixes` — сколько пар пришлось чинить.
#[derive(Debug, Clone, PartialEq)]
pub struct Repaired {
    pub events: Vec<Event>,
    pub fixes: u32,
}

/// Сегмент к рендеру: строка, снимки и лог до `upto` включительно.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub row: SegmentRow,
    pub snapshot: SegmentSnapshot,
    pub events: Vec<Event>,
}

/// Читает сегмент; нечитаемое событие или снимок — `Aux::Internal` (`01 §1.2`).
pub fn load(
    conn: &Connection,
    segment: SegmentId,
    upto: Option<EventId>,
) -> Result<Option<Loaded>, Aux> {
    let Some(row) = conn.turns().segment(segment)? else {
        return Ok(None);
    };
    let snapshot = SegmentSnapshot::from_row(&row)?;
    let events = decode_all(conn.turns().events_of_segment(segment, upto)?)?;
    Ok(Some(Loaded {
        row,
        snapshot,
        events,
    }))
}
