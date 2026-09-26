//! События (`06 §7`): `notify events.publish` подписчикам, порядок —
//! порядок публикации, `seq` на сессию, реплея нет.

use std::{borrow::Cow, fmt};

use eva::{
    _priv::schemars::{JsonSchema, Schema, SchemaGenerator, json_schema},
    data,
    time::{HumanDuration, Timestamp},
};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, DeserializeSeed, IgnoredAny, MapAccess, Visitor},
    ser::SerializeStruct,
};
use serde_json::Value;

use crate::types::{
    ChatId, CronJobId, MemoryId, ModuleId, RunId, SurfaceId, ToolId, TurnId, sleep::Phase,
    turn::Actor,
};

/// Params `events.publish`; на проводе `{topic, seq, at, payload}` (`06 §4`),
/// в msgpack `topic` раньше `payload`.
#[data(not(serde, schemars))]
pub struct Publish {
    pub seq: u64,
    pub at: Timestamp,
    pub event: Event,
}

impl Serialize for Publish {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("Publish", 4)?;
        s.serialize_field("topic", &self.event.topic())?;
        s.serialize_field("seq", &self.seq)?;
        s.serialize_field("at", &self.at)?;
        s.serialize_field("payload", &Payload(&self.event))?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for Publish {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let human_readable = deserializer.is_human_readable();
        deserializer.deserialize_struct(
            "Publish",
            PUBLISH_FIELDS,
            PublishVisitor { human_readable },
        )
    }
}

impl JsonSchema for Publish {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Publish")
    }

    fn schema_id() -> Cow<'static, str> {
        Cow::Borrowed(concat!(module_path!(), "::Publish"))
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let seq = generator.subschema_for::<u64>();
        let at = generator.subschema_for::<Timestamp>();
        let mut variants = vec![json_schema!({
            "properties": {"topic": {"const": Topic::Unknown.as_str()}, "payload": {}},
        })];
        macro_rules! variants {
            ($($topic:ident => $payload:ty),* $(,)?) => {$(
                let payload = generator.subschema_for::<$payload>();
                variants.push(json_schema!({
                    "properties": {
                        "topic": {"const": Topic::$topic.as_str()},
                        "payload": payload,
                    },
                }));
            )*};
        }
        variants! {
            TurnStarted => TurnEvent,
            TurnFinished => TurnEvent,
            ToolCalled => ToolCalled,
            ChatCreated => ChatEvent,
            ChatDeleted => ChatEvent,
            ChatRestored => ChatEvent,
            ChatPurged => ChatPurged,
            Notify => NotifyEvent,
            MemoryChanged => MemoryChanged,
            CronFired => CronFired,
            SleepPhase => SleepPhase,
            ModuleConnected => ModuleEvent,
            ModuleDisconnected => ModuleEvent,
        }
        json_schema!({
            "description": "Params of `events.publish`: `payload` is shaped by `topic`",
            "type": "object",
            "properties": {
                "topic": {"type": "string"},
                "seq": seq,
                "at": at,
                "payload": {},
            },
            "required": ["topic", "seq", "at", "payload"],
            "oneOf": variants,
        })
    }
}

const PUBLISH_FIELDS: &[&str] = &["topic", "seq", "at", "payload"];

/// Тело события без тега: тег уже лежит в `topic`.
struct Payload<'a>(&'a Event);

impl Serialize for Payload<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Event::TurnStarted(p) | Event::TurnFinished(p) => p.serialize(serializer),
            Event::ToolCalled(p) => p.serialize(serializer),
            Event::ChatCreated(p) | Event::ChatDeleted(p) | Event::ChatRestored(p) => {
                p.serialize(serializer)
            }
            Event::ChatPurged(p) => p.serialize(serializer),
            Event::Notify(p) => p.serialize(serializer),
            Event::MemoryChanged(p) => p.serialize(serializer),
            Event::CronFired(p) => p.serialize(serializer),
            Event::SleepPhase(p) => p.serialize(serializer),
            Event::ModuleConnected(p) | Event::ModuleDisconnected(p) => p.serialize(serializer),
            Event::Unknown => serializer.serialize_unit(),
        }
    }
}

/// Разбирает `payload` по уже прочитанному `topic`, без буфера.
struct PayloadSeed(Topic);

impl<'de> DeserializeSeed<'de> for PayloadSeed {
    type Value = Event;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Event, D::Error> {
        Ok(match self.0 {
            Topic::TurnStarted => Event::TurnStarted(Deserialize::deserialize(d)?),
            Topic::TurnFinished => Event::TurnFinished(Deserialize::deserialize(d)?),
            Topic::ToolCalled => Event::ToolCalled(Deserialize::deserialize(d)?),
            Topic::ChatCreated => Event::ChatCreated(Deserialize::deserialize(d)?),
            Topic::ChatDeleted => Event::ChatDeleted(Deserialize::deserialize(d)?),
            Topic::ChatRestored => Event::ChatRestored(Deserialize::deserialize(d)?),
            Topic::ChatPurged => Event::ChatPurged(Deserialize::deserialize(d)?),
            Topic::Notify => Event::Notify(Deserialize::deserialize(d)?),
            Topic::MemoryChanged => Event::MemoryChanged(Deserialize::deserialize(d)?),
            Topic::CronFired => Event::CronFired(Deserialize::deserialize(d)?),
            Topic::SleepPhase => Event::SleepPhase(Deserialize::deserialize(d)?),
            Topic::ModuleConnected => Event::ModuleConnected(Deserialize::deserialize(d)?),
            Topic::ModuleDisconnected => Event::ModuleDisconnected(Deserialize::deserialize(d)?),
            Topic::Unknown => {
                IgnoredAny::deserialize(d)?;
                Event::Unknown
            }
        })
    }
}

struct PublishVisitor {
    human_readable: bool,
}

impl<'de> Visitor<'de> for PublishVisitor {
    type Value = Publish;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("events.publish params {topic, seq, at, payload}")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Publish, A::Error> {
        let (mut topic, mut seq, mut at, mut event) = (None, None, None, None);
        let mut early = None;
        while let Some(key) = map.next_key::<Cow<'de, str>>()? {
            match &*key {
                "topic" => topic = Some(map.next_value::<Topic>()?),
                "seq" => seq = Some(map.next_value()?),
                "at" => at = Some(map.next_value()?),
                "payload" => match topic {
                    Some(topic) => event = Some(map.next_value_seed(PayloadSeed(topic))?),
                    // JSON-карты пиров бывают отсортированы; `Value` без потерь только там, бинарный `Timestamp` он ломает.
                    None if self.human_readable => early = Some(map.next_value::<Value>()?),
                    None => {
                        return Err(de::Error::custom(
                            "events.publish: `topic` must precede `payload`",
                        ));
                    }
                },
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }

        if let (None, Some(payload)) = (&event, early) {
            let topic = topic.ok_or_else(|| de::Error::missing_field("topic"))?;
            event = Some(
                PayloadSeed(topic)
                    .deserialize(payload)
                    .map_err(de::Error::custom)?,
            );
        }

        Ok(Publish {
            seq: seq.ok_or_else(|| de::Error::missing_field("seq"))?,
            at: at.ok_or_else(|| de::Error::missing_field("at"))?,
            event: event.ok_or_else(|| de::Error::missing_field("payload"))?,
        })
    }
}

/// Подписка capability `events` и ключ доставки `Policy`.
#[data(copy, ord)]
#[derive(Hash)]
#[serde(remote = "Topic")]
pub enum Topic {
    #[serde(rename = "turn.started")]
    TurnStarted,
    #[serde(rename = "turn.finished")]
    TurnFinished,
    #[serde(rename = "tool.called")]
    ToolCalled,
    #[serde(rename = "chat.created")]
    ChatCreated,
    #[serde(rename = "chat.deleted")]
    ChatDeleted,
    #[serde(rename = "chat.restored")]
    ChatRestored,
    #[serde(rename = "chat.purged")]
    ChatPurged,
    /// Только `client`: тело — `chats.history_read` чата `notify`.
    Notify,
    #[serde(rename = "memory.changed")]
    MemoryChanged,
    #[serde(rename = "cron.fired")]
    CronFired,
    #[serde(rename = "sleep.phase")]
    SleepPhase,
    #[serde(rename = "module.connected")]
    ModuleConnected,
    #[serde(rename = "module.disconnected")]
    ModuleDisconnected,
    /// Топик новее этого пира.
    #[serde(other)]
    Unknown,
}

tolerant!(Topic);

impl Topic {
    /// Имя на проводе.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TurnStarted => "turn.started",
            Self::TurnFinished => "turn.finished",
            Self::ToolCalled => "tool.called",
            Self::ChatCreated => "chat.created",
            Self::ChatDeleted => "chat.deleted",
            Self::ChatRestored => "chat.restored",
            Self::ChatPurged => "chat.purged",
            Self::Notify => "notify",
            Self::MemoryChanged => "memory.changed",
            Self::CronFired => "cron.fired",
            Self::SleepPhase => "sleep.phase",
            Self::ModuleConnected => "module.connected",
            Self::ModuleDisconnected => "module.disconnected",
            Self::Unknown => "unknown",
        }
    }
}

impl fmt::Display for Topic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[data]
#[serde(remote = "Event")]
pub enum Event {
    #[serde(rename = "turn.started")]
    TurnStarted(TurnEvent),
    #[serde(rename = "turn.finished")]
    TurnFinished(TurnEvent),
    #[serde(rename = "tool.called")]
    ToolCalled(ToolCalled),
    #[serde(rename = "chat.created")]
    ChatCreated(ChatEvent),
    #[serde(rename = "chat.deleted")]
    ChatDeleted(ChatEvent),
    #[serde(rename = "chat.restored")]
    ChatRestored(ChatEvent),
    #[serde(rename = "chat.purged")]
    ChatPurged(ChatPurged),
    Notify(NotifyEvent),
    #[serde(rename = "memory.changed")]
    MemoryChanged(MemoryChanged),
    #[serde(rename = "cron.fired")]
    CronFired(CronFired),
    #[serde(rename = "sleep.phase")]
    SleepPhase(SleepPhase),
    #[serde(rename = "module.connected")]
    ModuleConnected(ModuleEvent),
    #[serde(rename = "module.disconnected")]
    ModuleDisconnected(ModuleEvent),
    /// Топик новее этого пира; payload пропущен.
    #[serde(other)]
    Unknown,
}

tolerant!(Event);

impl Event {
    pub const fn topic(&self) -> Topic {
        match self {
            Self::TurnStarted(_) => Topic::TurnStarted,
            Self::TurnFinished(_) => Topic::TurnFinished,
            Self::ToolCalled(_) => Topic::ToolCalled,
            Self::ChatCreated(_) => Topic::ChatCreated,
            Self::ChatDeleted(_) => Topic::ChatDeleted,
            Self::ChatRestored(_) => Topic::ChatRestored,
            Self::ChatPurged(_) => Topic::ChatPurged,
            Self::Notify(_) => Topic::Notify,
            Self::MemoryChanged(_) => Topic::MemoryChanged,
            Self::CronFired(_) => Topic::CronFired,
            Self::SleepPhase(_) => Topic::SleepPhase,
            Self::ModuleConnected(_) => Topic::ModuleConnected,
            Self::ModuleDisconnected(_) => Topic::ModuleDisconnected,
            Self::Unknown => Topic::Unknown,
        }
    }
}

/// `turn.started` / `turn.finished`.
#[data]
pub struct TurnEvent {
    pub turn_id: TurnId,
    pub chat: ChatId,
    pub surface: SurfaceId,
    pub actor: Actor,
    pub run_id: Option<RunId>,
    /// Только у `turn.finished`.
    pub outcome: Option<TurnOutcome>,
}

/// Исход тура — `turn_ended.outcome` лога.
#[data(copy, ord, display(name))]
#[serde(remote = "TurnOutcome")]
pub enum TurnOutcome {
    Completed,
    Cancelled,
    Failed,
    Interrupted,
    /// Исход новее этого пира.
    #[serde(other)]
    Unknown,
}

tolerant!(TurnOutcome);

#[data]
pub struct ToolCalled {
    pub turn_id: TurnId,
    pub chat: ChatId,
    /// `None` у `extra_tools` тура.
    pub tool_id: Option<ToolId>,
    pub canonical: String,
    pub is_error: bool,
    pub duration: HumanDuration,
}

/// `chat.created` / `chat.deleted` / `chat.restored`.
#[data]
pub struct ChatEvent {
    pub chat: ChatId,
    pub surface: SurfaceId,
    /// Внешний id чата у поверхности.
    pub external_id: Option<String>,
}

/// `service` получает только чаты своих поверхностей: ядро шлёт по событию на surface.
#[data]
pub struct ChatPurged {
    pub ids: Vec<ChatId>,
}

/// Без тела: его читают `chats.history_read` чата `notify`.
#[data]
pub struct NotifyEvent {
    pub subject: String,
    pub chat_id: ChatId,
    pub source_chat: Option<ChatId>,
}

#[data]
pub struct MemoryChanged {
    pub ids: Vec<MemoryId>,
    pub op: MemoryOp,
}

#[data(copy, ord, display(name))]
#[serde(remote = "MemoryOp")]
pub enum MemoryOp {
    Remember,
    Supersede,
    Forget,
    Restore,
    /// Сон: слияние, расщепление, понижение (`ArchiveReason`).
    Merge,
    Split,
    Demote,
    /// Операция новее этого пира.
    #[serde(other)]
    Unknown,
}

tolerant!(MemoryOp);

#[data]
pub struct CronFired {
    pub job: CronJobId,
    pub run_id: RunId,
    pub chat: ChatId,
}

#[data]
pub struct SleepPhase {
    pub phase: Phase,
    pub run_id: RunId,
    pub state: PhaseState,
}

#[data(copy, ord, display(name))]
#[serde(remote = "PhaseState")]
pub enum PhaseState {
    Started,
    Finished,
    /// Состояние новее этого пира.
    #[serde(other)]
    Unknown,
}

tolerant!(PhaseState);

/// `module.connected` / `module.disconnected`.
#[data]
pub struct ModuleEvent {
    pub module: ModuleId,
}
