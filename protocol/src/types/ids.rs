//! Идентификаторы.
//!
//! Правило одно: **serde-форма равна форме хранения**, а `Display` даёт
//! форму, которую видит модель. У целочисленных id это `c42` при числе 42 в
//! базе и на проводе; у строковых форма хранения и есть человеческая
//! (`p7`, `local`). Иначе клиенту пришлось бы парсить префиксы, а
//! префикс в промпте экономит токены на каждом упоминании.

use std::{borrow::Cow, fmt, marker::PhantomData, str::FromStr};

use eva::{
    _priv::schemars::{JsonSchema, Schema, SchemaGenerator, json_schema},
    data,
};
use serde::de::{self, Visitor};

/// Целочисленный id: на проводе число, модели — `<pfx><N>`.
///
/// На входе принимает и `42`, и `"42"`, и `"<pfx>42"` (`11 §7`): модель
/// пишет то, что видит в промпте.
macro_rules! int_id {
    ($(#[$meta:meta])* $name:ident($repr:ty) = $pfx:literal;) => {
        $(#[$meta])*
        #[data(copy, ord, not(Deserialize, schemars))]
        #[derive(Hash)]
        #[repr(transparent)]
        #[serde(transparent)]
        pub struct $name(pub $repr);

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> ::core::result::Result<Self, D::Error> {
                deserializer
                    .deserialize_any(IntIdVisitor::<$repr>::new(stringify!($name), $pfx))
                    .map(Self)
            }
        }

        impl JsonSchema for $name {
            fn schema_name() -> Cow<'static, str> {
                Cow::Borrowed(stringify!($name))
            }

            fn schema_id() -> Cow<'static, str> {
                Cow::Borrowed(concat!(module_path!(), "::", stringify!($name)))
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({
                    "description": concat!(
                        "Integer id: a number on output; input also accepts \"42\" and \"",
                        $pfx,
                        "42\"",
                    ),
                    "anyOf": [
                        {"type": "integer", "format": "int64"},
                        {"type": "string", "pattern": concat!("^(", $pfx, ")?[+-]?[0-9]+$")},
                    ],
                })
            }
        }

        impl $name {
            pub const fn get(self) -> $repr {
                self.0
            }

            /// Префикс, с которым id показывается модели.
            pub const PREFIX: &'static str = $pfx;
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                write!(f, concat!($pfx, "{}"), self.0)
            }
        }

        impl From<$repr> for $name {
            fn from(value: $repr) -> Self {
                Self(value)
            }
        }
    };
}

/// Число или строка `[<pfx>]<N>`.
struct IntIdVisitor<T> {
    name: &'static str,
    prefix: &'static str,
    repr: PhantomData<T>,
}

impl<T> IntIdVisitor<T> {
    const fn new(name: &'static str, prefix: &'static str) -> Self {
        Self {
            name,
            prefix,
            repr: PhantomData,
        }
    }
}

impl<T> Visitor<'_> for IntIdVisitor<T>
where
    T: TryFrom<i64> + TryFrom<u64> + FromStr,
{
    type Value = T;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: an integer, \"42\" or \"{}42\"",
            self.name, self.prefix
        )
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<T, E> {
        T::try_from(v).map_err(|_| E::invalid_value(de::Unexpected::Signed(v), &self))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<T, E> {
        T::try_from(v).map_err(|_| E::invalid_value(de::Unexpected::Unsigned(v), &self))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<T, E> {
        v.strip_prefix(self.prefix)
            .unwrap_or(v)
            .parse()
            .map_err(|_| E::invalid_value(de::Unexpected::Str(v), &self))
    }
}

/// Строковый id: форма хранения и человеческая совпадают.
macro_rules! str_id {
    ($(#[$meta:meta])* $name:ident;) => {
        $(#[$meta])*
        #[data(ord)]
        #[derive(Hash)]
        #[repr(transparent)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                f.write_str(self.0.as_str())
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(String::from(value))
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }
    };
}

int_id! {
    /// Чат.
    ChatId(i64) = "c";
}

int_id! {
    /// Папка внутри surface.
    FolderId(i64) = "f";
}

int_id! {
    /// Событие лога сегмента; он же `seq` ленты items.
    EventId(i64) = "e";
}

int_id! {
    /// Сегмент чата.
    SegmentId(i64) = "s";
}

int_id! {
    /// Результат вызова тула, per-chat счётчик: `t17` в промпте.
    Tseq(i64) = "t";
}

int_id! {
    /// Запись памяти.
    MemoryId(i64) = "m";
}

int_id! {
    /// Инструмент в реестре. Модели не виден — ключ статистики и оверрайдов.
    ToolId(i64) = "tool";
}

str_id! {
    /// Личность: `p<N>`, TEXT в базе.
    PersonId;
}

str_id! {
    /// Surface: slug, его же объявляет модуль.
    SurfaceId;
}

str_id! {
    /// Тур: uuid v7, живёт в памяти soul и в ссылках `spend`/pending.
    TurnId;
}

str_id! {
    /// Blob файла или картинки.
    BlobId;
}

str_id! {
    /// Модуль: имя из `hello`, оно же скоуп `fs`/`sqlite`/`kv`.
    ModuleId;
}

str_id! {
    /// Модель инференса, как её объявил провайдер.
    ModelId;
}

impl PersonId {
    /// `p<N>` — единственная законная форма личности, кроме `p0` (`system`).
    pub fn numbered(n: u32) -> Self {
        Self(format!("p{n}"))
    }

    /// Актор soul: крон, сон, уведомления, служебные вставки.
    pub fn system() -> Self {
        Self(String::from("p0"))
    }
}

int_id! {
    /// Постоянный грант рук (`hands_grants`).
    HandsGrantId(i64) = "g";
}

str_id! {
    /// Запуск крона или ночи сна: uuid v7.
    RunId;
}

str_id! {
    /// Pending-запрос `ask`/`permission`: uuid v7.
    PendingId;
}

str_id! {
    /// Джоба крона: uuid v7.
    CronJobId;
}

str_id! {
    /// Сессия модуля: uuid v7; `hello.resume` предъявляет её после реконнекта.
    SessionId;
}

/// Сообщение адресуется событием лога: `events.id` уникален глобально, в отличие от `seq` чата.
pub type MessageId = EventId;
