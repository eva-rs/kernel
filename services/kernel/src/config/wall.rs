//! Время суток и день недели в конфиге: `HH:MM` и `<weekday> HH:MM` зоны
//! `system.tz` (`00 §5`).

use std::{fmt, str::FromStr};

use eva::{
    _priv::schemars::{JsonSchema, Schema, SchemaGenerator, json_schema},
    data,
    time::Time,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// `eva::time::Time` строкой `HH:MM`.
#[data(copy, ord, not(serde, JsonSchema))]
#[derive(Hash)]
pub struct WallTime(pub Time);

impl fmt::Display for WallTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.to_str().fmt(f)
    }
}

impl FromStr for WallTime {
    type Err = ParseWall;

    fn from_str(s: &str) -> Result<Self, ParseWall> {
        s.parse::<Time>()
            .map(Self)
            .map_err(|_| ParseWall::new(s, "expected HH:MM"))
    }
}

impl Serialize for WallTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for WallTime {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = std::borrow::Cow::<str>::deserialize(deserializer)?;
        s.parse().map_err(de::Error::custom)
    }
}

impl JsonSchema for WallTime {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("WallTime")
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed(concat!(module_path!(), "::WallTime"))
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^([01][0-9]|2[0-3]):[0-5][0-9]$",
            "description": "Time of day HH:MM in system.tz",
        })
    }
}

/// День недели, как пишут в конфиге: `mon`…`sun`.
#[data(copy, ord, display(name))]
#[derive(Hash)]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Weekday {
    const NAMES: [(&'static str, Self); 7] = [
        ("mon", Self::Mon),
        ("tue", Self::Tue),
        ("wed", Self::Wed),
        ("thu", Self::Thu),
        ("fri", Self::Fri),
        ("sat", Self::Sat),
        ("sun", Self::Sun),
    ];

    pub fn as_str(self) -> &'static str {
        Self::NAMES
            .iter()
            .find(|(_, day)| *day == self)
            .map_or("sun", |(name, _)| name)
    }

    pub fn to_jiff(self) -> jiff::civil::Weekday {
        use jiff::civil::Weekday as J;
        match self {
            Self::Mon => J::Monday,
            Self::Tue => J::Tuesday,
            Self::Wed => J::Wednesday,
            Self::Thu => J::Thursday,
            Self::Fri => J::Friday,
            Self::Sat => J::Saturday,
            Self::Sun => J::Sunday,
        }
    }
}

/// `<weekday> HH:MM`: еженедельный слот.
#[data(copy, ord, not(serde, JsonSchema))]
#[derive(Hash)]
pub struct WeeklyAt {
    pub weekday: Weekday,
    pub time: WallTime,
}

impl fmt::Display for WeeklyAt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.weekday.as_str(), self.time)
    }
}

impl FromStr for WeeklyAt {
    type Err = ParseWall;

    fn from_str(s: &str) -> Result<Self, ParseWall> {
        let (day, time) = s
            .trim()
            .split_once(char::is_whitespace)
            .ok_or_else(|| ParseWall::new(s, "expected <weekday> HH:MM"))?;
        let weekday = Weekday::NAMES
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(day))
            .map(|(_, weekday)| *weekday)
            .ok_or_else(|| ParseWall::new(s, "weekday must be mon..sun"))?;
        Ok(Self {
            weekday,
            time: time.trim().parse()?,
        })
    }
}

impl Serialize for WeeklyAt {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for WeeklyAt {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = std::borrow::Cow::<str>::deserialize(deserializer)?;
        s.parse().map_err(de::Error::custom)
    }
}

impl JsonSchema for WeeklyAt {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("WeeklyAt")
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed(concat!(module_path!(), "::WeeklyAt"))
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^(mon|tue|wed|thu|fri|sat|sun) ([01][0-9]|2[0-3]):[0-5][0-9]$",
            "description": "Weekly slot `<weekday> HH:MM` in system.tz",
        })
    }
}

#[data(
    error,
    not(serde, JsonSchema),
    display("invalid time {input:?}: {reason}")
)]
pub struct ParseWall {
    pub input: String,
    pub reason: String,
}

impl ParseWall {
    fn new(input: &str, reason: &str) -> Self {
        Self {
            input: input.to_owned(),
            reason: reason.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_prints_wall_times() {
        let at: WallTime = "03:07".parse().expect("time");
        assert_eq!(at.to_string(), "03:07");
        assert!("3:07".parse::<WallTime>().is_err());
        assert!("24:00".parse::<WallTime>().is_err());

        let weekly: WeeklyAt = "Sun 03:00".parse().expect("weekly");
        assert_eq!(weekly.weekday, Weekday::Sun);
        assert_eq!(weekly.to_string(), "sun 03:00");
        assert!("someday 03:00".parse::<WeeklyAt>().is_err());
        assert!("sun".parse::<WeeklyAt>().is_err());

        let json = serde_json::to_string(&weekly).expect("json");
        assert_eq!(json, "\"sun 03:00\"");
        assert_eq!(
            serde_json::from_str::<WeeklyAt>(&json).expect("back"),
            weekly
        );
    }
}
