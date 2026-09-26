//! Глубина размышления. Наследуется surface → folder → chat → message.

use eva::data;

/// На проводе — имена, в базе — `as_u8()` (INTEGER 1..=4).
#[data(copy, ord)]
#[derive(Hash, Default)]
pub enum Effort {
    Low,
    #[default]
    Medium,
    High,
    Max,
}

impl Effort {
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
            Self::Max => 4,
        }
    }

    pub const fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Low,
            2 => Self::Medium,
            3 => Self::High,
            4 => Self::Max,
            _ => return None,
        })
    }
}
