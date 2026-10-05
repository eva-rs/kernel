//! Чат: последовательность сообщений с одной историей.

use std::fmt;

use eva::{data, time::Timestamp};

use crate::types::{
    ChatId, Effort, EventId, FolderId, ModelId, PersonId, SegmentId, SurfaceId, Thinking, TurnId,
    content::Block,
};

#[data]
pub struct Chat {
    pub id: ChatId,
    pub folder_id: FolderId,
    pub surface_id: SurfaceId,
    pub kind: Kind,
    /// Своё или выведенное моделью из первой реплики.
    pub title: Option<String>,
    /// От поверхности; при показе побеждает: `external_title ?? title ?? c<N>`.
    pub external_title: Option<String>,
    /// Внешний адрес чата у поверхности.
    pub alias: Option<Alias>,
    pub pinned: bool,
    /// Ева просит внимания хозяина.
    pub alerted: bool,
    pub undeletable: bool,
    pub deleted_at: Option<Timestamp>,
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
    pub thinking: Option<Thinking>,
    pub compact_at: Option<f32>,
    /// Явный trust; эффективный считается по цепочке surface → folder → chat.
    pub trusted: Option<bool>,
    /// Эффективный trust на момент чтения — считает ядро.
    pub trusted_chat: bool,
    pub project: Option<String>,
    pub workdir: Option<String>,
    /// Имя endpoint'а рук; `None` — дефолтный из конфига.
    pub workdir_on: Option<String>,
    /// Открытый тур: по нему клиент переподписывается после реконнекта.
    pub live_turn: Option<TurnId>,
    pub created_at: Timestamp,
    pub last_activity_at: Timestamp,
}

/// Эфемерный чат уносит GC; переход только в одну сторону.
#[data(copy, ord)]
pub enum Kind {
    Ephemeral,
    Persistent,
}

/// Вид внешнего чата — от него зависит trust ЛС хозяина.
#[data(copy, ord)]
pub enum ExternalKind {
    Direct,
    Group,
    Channel,
}

#[data]
pub struct Alias {
    pub external_id: String,
    pub kind: ExternalKind,
}

/// Чат по id или по внешнему адресу поверхности.
///
/// Untagged: на проводе `42` либо `{"surface": "x", "external_id": "…"}`.
#[data]
#[serde(untagged)]
pub enum Selector {
    Id(ChatId),
    Alias {
        surface: SurfaceId,
        external_id: String,
    },
}

impl From<ChatId> for Selector {
    fn from(value: ChatId) -> Self {
        Self::Id(value)
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Id(id) => id.fmt(f),
            Self::Alias {
                surface,
                external_id,
            } => write!(f, "{surface}:{external_id}"),
        }
    }
}

#[data(copy, ord)]
pub enum Role {
    User,
    Assistant,
}

/// Реплика истории чата.
#[data]
pub struct Message {
    pub id: EventId,
    /// Номер реплики в чате: `#<seq>` в поиске и `history_read`.
    pub seq: u64,
    pub role: Role,
    /// `None` у `user` — неизвестный отправитель, никогда не хозяин.
    pub sender: Option<PersonId>,
    pub blocks: Vec<Block>,
    pub external_id: Option<String>,
    pub reply_to_external_id: Option<String>,
    /// `None` — silent-append без тура.
    pub turn: Option<TurnId>,
    pub created_at: Timestamp,
}

/// Строка результата `chats.search`: реплика либо название чата.
#[data]
pub struct SearchHit {
    pub chat: ChatId,
    pub title: Option<String>,
    /// `None` — попало название чата, не реплика.
    pub seq: Option<u64>,
    pub role: Option<Role>,
    pub sender: Option<PersonId>,
    /// Через `sanitize_inline`, не длиннее `chats.search.snippet_tokens`.
    pub snippet: String,
    pub at: Timestamp,
    /// RRF-оценка: больше — выше.
    pub score: f64,
}

/// Сегмент чата: один байт-стабильный префикс.
#[data]
pub struct Segment {
    pub id: SegmentId,
    pub chat: ChatId,
    pub seq: u32,
    pub reason: SegmentReason,
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
    pub summary: Option<String>,
}

#[data(copy, ord)]
pub enum SegmentReason {
    Initial,
    Compact,
    Downshift,
    Overflow,
    Manual,
    Reset,
}
