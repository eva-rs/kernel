//! Межчатовая память: плоские записи с версиями и контракты её тулов.

use eva::{data, time::Timestamp};

use crate::types::{ChatId, MemoryId, ModelId, PersonId, RunId, SurfaceId};

#[data(copy, ord, display(name))]
#[derive(Hash)]
pub enum Kind {
    Fact,
    Event,
    Core,
    Procedure,
}

/// `public ⊂ trusted ⊂ private`.
#[data(copy, ord, display(name))]
#[derive(Hash)]
pub enum Visibility {
    Public,
    Trusted,
    Private,
}

/// `importance` на проводе тулов: в БД `/100`.
#[eva::int(u8, 0..=100)]
pub enum Percent {}

impl Percent {
    pub fn from_unit(value: f64) -> Self {
        let scaled = (value.clamp(0.0, 1.0) * 100.0).round();
        // Округление 0..=1 к 0..=100 не покидает диапазон; `POS50` — страховка от NaN.
        Self::new(scaled as u8).unwrap_or(Self::POS50)
    }

    pub fn to_unit(self) -> f64 {
        f64::from(self.into_inner()) / 100.0
    }
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

#[data(copy, ord, display(name))]
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

/// Контракты тулов группы `memory`: вход — схема тула, выход — его результат.
pub mod tools {
    use super::*;

    /// Что сделал `memory_remember`.
    #[data(copy, ord, display(name))]
    pub enum Action {
        Inserted,
        /// Дубль с другим текстом или метаданными слит в одну запись.
        Merged,
        /// Байт-в-байт тот же текст: только подкрепление.
        Reinforced,
    }

    /// Соседняя запись выше порога близости: повод свериться.
    #[data]
    pub struct Neighbour {
        pub id: MemoryId,
        pub kind: Kind,
        pub age: String,
        pub text: String,
    }

    pub mod remember {
        use super::*;

        #[data]
        pub struct Args {
            pub kind: Kind,
            pub text: String,
            /// `None` — дефолт по аудитории тура.
            pub visibility: Option<Visibility>,
            pub importance: Option<Percent>,
            /// Только `event`: когда произошло или произойдёт.
            pub occurred_at: Option<Timestamp>,
            pub valid_from: Option<Timestamp>,
            #[serde(default)]
            pub tags: Vec<String>,
            /// Обязательны при `procedure`.
            pub name: Option<String>,
            pub description: Option<String>,
        }

        #[data]
        pub struct Ok {
            pub id: MemoryId,
            pub action: Action,
            /// До `memory.dedup.neighbours` записей выше `neighbour_floor`, кроме самой.
            pub neighbours: Vec<Neighbour>,
        }
    }

    pub mod supersede {
        use super::*;

        #[data]
        pub struct Args {
            pub id: MemoryId,
            pub text: String,
            pub kind: Option<Kind>,
            pub visibility: Option<Visibility>,
            pub importance: Option<Percent>,
            pub occurred_at: Option<Timestamp>,
            pub valid_from: Option<Timestamp>,
            pub tags: Option<Vec<String>>,
            pub name: Option<String>,
            pub description: Option<String>,
        }

        #[data]
        pub struct Ok {
            /// Новая версия.
            pub id: MemoryId,
            pub supersedes: MemoryId,
            pub version: u32,
        }
    }

    pub mod forget {
        use super::*;

        #[data]
        pub struct Args {
            pub id: MemoryId,
            pub why: String,
        }

        #[data]
        pub struct Ok {
            pub id: MemoryId,
            /// Пока GC не унёс архив, `memory.restore` вернёт запись.
            pub archived_until: Timestamp,
        }
    }

    pub mod recall {
        use super::*;

        #[data]
        pub struct Args {
            pub query: String,
            pub kind: Option<Kind>,
            /// Версии, истинные в этот момент; `None` — текущие.
            pub at: Option<Timestamp>,
            /// Только `event`: `occurred_at ∈ [since, until]`.
            pub since: Option<Timestamp>,
            pub until: Option<Timestamp>,
            /// `None` — `memory.recall.limit`; потолок 25.
            pub limit: Option<u8>,
        }

        #[data]
        pub struct Hit {
            pub id: MemoryId,
            pub kind: Kind,
            pub age: String,
            pub importance: Percent,
            pub visibility: Visibility,
            pub text: String,
        }

        #[data]
        pub struct Ok {
            pub hits: Vec<Hit>,
            /// Фильтр видимости что-то отсёк.
            pub withheld: bool,
        }
    }

    /// `memory_read` и `memory_history` — обёртки `memory.get`/`memory.history` с `view`.
    pub mod read {
        use super::*;

        #[data]
        pub struct Args {
            pub id: MemoryId,
        }
    }
}
