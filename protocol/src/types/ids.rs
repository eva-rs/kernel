//! Идентификаторы.
//!
//! Правило одно: **serde-форма равна форме хранения**, а `Display` даёт
//! форму, которую видит модель. У целочисленных id это `c42` при числе 42 в
//! базе и на проводе; у строковых форма хранения и есть человеческая
//! (`p7`, `telegram`). Иначе клиенту пришлось бы парсить префиксы, а
//! префикс в промпте экономит токены на каждом упоминании (legacy: uuid в
//! маркерах раздувал контекст).

use eva::data;

/// Целочисленный id: на проводе число, модели — `<pfx><N>`.
macro_rules! int_id {
    ($(#[$meta:meta])* $name:ident($repr:ty) = $pfx:literal;) => {
        $(#[$meta])*
        #[data(copy, ord)]
        #[derive(Hash)]
        #[repr(transparent)]
        #[serde(transparent)]
        pub struct $name(pub $repr);

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
    /// Тур: uuid v7, живёт в памяти ядра и в ссылках `spend`/pending.
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

    /// Актор ядра: крон, сон, уведомления, служебные вставки.
    pub fn system() -> Self {
        Self(String::from("p0"))
    }
}
