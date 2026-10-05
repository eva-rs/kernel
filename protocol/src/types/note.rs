//! Блокнот чата: заметки, режим секции и контракты тулов группы `notes`.

use eva::{data, time::Timestamp};

use crate::types::SegmentId;

#[data]
pub struct Note {
    pub name: String,
    pub description: String,
    pub body: String,
    /// Тело до последней правки — клиенты рисуют дифф.
    pub prev_body: Option<String>,
    pub pinned: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[data(copy, ord, display(name))]
#[derive(Hash)]
pub enum Mode {
    Inline,
    Squeeze,
    Index,
}

/// Строка `notes_state` чата; режим меняется только на входе в тур.
#[data]
pub struct State {
    pub mode: Mode,
    pub since_at: Timestamp,
    /// Master-туров, проведённых в `squeeze`; вне `squeeze` = 0.
    pub squeeze_turns: u32,
    pub wrap_up_segment: Option<SegmentId>,
    pub nudge_segment: Option<SegmentId>,
    pub seen_at: Option<Timestamp>,
}

/// Заполненность блокнота после мутации или на входе в тур.
#[data(copy)]
pub struct Fill {
    /// Токены inline-рендера секции.
    pub tokens: u32,
    pub budget: u32,
    pub count: u32,
}

impl Fill {
    /// Проценты бюджета, целые.
    pub fn percent(self) -> u32 {
        if self.budget == 0 {
            return 0;
        }
        let share = u64::from(self.tokens) * 100 / u64::from(self.budget);
        u32::try_from(share).unwrap_or(u32::MAX)
    }
}

/// Событие гигиены `note_updated` лога сегмента.
#[data]
pub struct NoteUpdated {
    pub name: String,
    pub op: Op,
    /// Фрагмент секции в режиме на момент события; пусто при `deleted`.
    pub entry: String,
}

#[data(copy, ord, display(name))]
pub enum Op {
    Saved,
    Updated,
    Deleted,
}

/// Контракты тулов группы `notes`.
pub mod tools {
    use super::*;

    pub mod save {
        use super::*;

        #[data]
        pub struct Args {
            pub name: String,
            pub description: String,
            pub body: String,
            /// `None` у существующей заметки — оставить как было.
            pub pinned: Option<bool>,
        }

        #[data]
        pub struct Ok {
            pub name: String,
            pub replaced: bool,
            pub fill: Fill,
        }
    }

    pub mod update {
        use super::*;

        #[data]
        pub struct Args {
            pub name: String,
            pub description: Option<String>,
            pub body: Option<String>,
            pub pinned: Option<bool>,
        }

        #[data]
        pub struct Ok {
            pub name: String,
            /// Имена изменённых полей.
            pub fields: Vec<String>,
            pub fill: Fill,
        }
    }

    pub mod delete {
        use super::*;

        #[data]
        pub struct Args {
            pub name: String,
        }

        #[data]
        pub struct Ok {
            pub name: String,
            pub fill: Fill,
        }
    }

    pub mod read {
        use super::*;

        #[data]
        pub struct Args {
            pub names: Vec<String>,
        }

        #[data]
        pub struct Ok {
            /// Тела в порядке секции плюс строки `- name: not found`.
            pub text: String,
        }
    }
}
