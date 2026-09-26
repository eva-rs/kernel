//! Межчатовая память: плоские записи с версиями.

use eva::{data, time::Timestamp};

use crate::types::{ChatId, MemoryId, ModelId, PersonId, RunId, SurfaceId};

#[data(copy, ord)]
#[derive(Hash)]
pub enum Kind {
    Fact,
    Event,
    Core,
    Procedure,
}

/// `public ⊂ trusted ⊂ private`.
#[data(copy, ord)]
#[derive(Hash)]
pub enum Visibility {
    Public,
    Trusted,
    Private,
}

#[data]
pub struct Record {
    pub id: MemoryId,
    pub kind: Kind,
    pub text: String,
    /// Только `procedure`.
    pub name: Option<String>,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub visibility: Visibility,
    pub importance: f32,
    pub confidence: f32,
    pub source: Provenance,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub consolidated_at: Option<Timestamp>,
    pub occurred_at: Option<Timestamp>,
    pub valid_from: Timestamp,
    /// `None` — текущая версия.
    pub valid_to: Option<Timestamp>,
    pub supersedes: Option<MemoryId>,
    pub version: u32,
    pub last_reinforced_at: Timestamp,
    pub access_count: f32,
    pub embed_model: Option<ModelId>,
}

/// Откуда запись: тур, в котором её сделали.
#[data]
#[derive(Default)]
pub struct Provenance {
    pub actor: Option<PersonId>,
    pub chat: Option<ChatId>,
    pub surface: Option<SurfaceId>,
    pub trusted_chat: Option<bool>,
    pub run: Option<RunId>,
}

#[data]
pub struct Archive {
    pub reason: ArchiveReason,
    pub why: String,
    /// `None` — рука хозяина, не сон.
    pub run: Option<RunId>,
    pub actor: Option<PersonId>,
    pub archived_at: Timestamp,
    pub restored_at: Option<Timestamp>,
}

#[data(copy, ord)]
pub enum ArchiveReason {
    Forget,
    Merge,
    Split,
    Demote,
}

/// Звено цепочки `memory.history`.
#[data]
pub struct Version {
    pub record: Record,
    pub archive: Option<Archive>,
}
