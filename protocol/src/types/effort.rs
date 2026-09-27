//! Глубина размышления. Наследуется surface → folder → chat → message.

use std::fmt;

use eva::data;
use serde::{
    Deserialize, Deserializer,
    de::{self, Unexpected, Visitor},
};

/// Пишется именем, читается и именем, и номером 1..=4 (`05 §2.1`); в базе — `as_u8()`.
#[data(copy, ord, not(Deserialize))]
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

    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "max" => Self::Max,
            _ => return None,
        })
    }
}

const NAMES: &[&str] = &["low", "medium", "high", "max"];

/// Не derive: тот читает число как индекс варианта с нуля, и msgpack `2` стал бы `High`.
impl<'de> Deserialize<'de> for Effort {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(EffortVisitor)
    }
}

struct EffortVisitor;

impl Visitor<'_> for EffortVisitor {
    type Value = Effort;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("low, medium, high, max or an integer 1..=4")
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Effort, E> {
        u8::try_from(v)
            .ok()
            .and_then(Effort::from_u8)
            .ok_or_else(|| E::invalid_value(Unexpected::Unsigned(v), &self))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Effort, E> {
        u8::try_from(v)
            .ok()
            .and_then(Effort::from_u8)
            .ok_or_else(|| E::invalid_value(Unexpected::Signed(v), &self))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Effort, E> {
        Effort::from_name(v).ok_or_else(|| E::unknown_variant(v, NAMES))
    }
}
