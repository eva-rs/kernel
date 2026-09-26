//! Чат: последовательность сообщений с одной историей.

use eva::{data, time::Timestamp};

use crate::types::{ChatId, Effort, FolderId, SurfaceId};

#[data]
pub struct Chat {
    pub id: ChatId,
    pub folder_id: FolderId,
    pub surface_id: SurfaceId,
    pub kind: Kind,
    /// Своё или выведенное моделью из первой реплики.
    pub title: Option<String>,
    /// От поверхности; при показе побеждает `title`.
    pub external_title: Option<String>,
    pub pinned: bool,
    /// Ева просит внимания хозяина.
    pub alerted: bool,
    pub undeletable: bool,
    pub deleted_at: Option<Timestamp>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub thinking: Option<bool>,
    /// Явный trust; эффективный считается по цепочке surface → folder → chat.
    pub trusted: Option<bool>,
    /// Эффективный trust на момент чтения — считает ядро.
    pub trusted_chat: bool,
    pub project: Option<String>,
    pub workdir: Option<String>,
    /// Имя endpoint'а рук; `None` — дефолтный из конфига.
    pub workdir_on: Option<String>,
    pub created_at: Timestamp,
    pub last_activity_at: Timestamp,
}

/// Эфемерный чат уносит GC; переход только в одну сторону.
#[data(copy, ord)]
pub enum Kind {
    Ephemeral,
    Persistent,
}
