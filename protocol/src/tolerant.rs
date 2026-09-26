//! Приём enum'ов от пира новее: незнакомый вариант → catch-all, соединение живо.
//!
//! Форма внешне-тегированная; голый `#[serde(other)]` падает на payload
//! незнакомого data-варианта (rmp ждёт `nil`), а внутренний тег буферизует
//! payload, где `Timestamp` из msgpack не читается. Поэтому derive отдаёт тело
//! через `#[serde(remote = "<Имя>")]`, а `tolerant!` читает enum без буфера и
//! пропускает payload незнакомого варианта.
//!
//! ```ignore
//! #[data]
//! #[serde(remote = "Topic")]
//! pub enum Topic {
//!     TurnStarted,
//!     /// Вариант новее этого пира.
//!     #[serde(other)]
//!     Unknown,
//! }
//!
//! tolerant!(Topic);
//! ```

use std::fmt;

use serde::de::{
    self, DeserializeSeed, Deserializer, EnumAccess, IgnoredAny, IntoDeserializer, MapAccess,
    VariantAccess, Visitor, value::BorrowedStrDeserializer,
};

/// `Serialize`/`Deserialize` поверх тел, которые derive положил в inherent
/// `serialize`/`deserialize` (`#[serde(remote = "<Имя>")]`).
macro_rules! tolerant {
    ($ty:ident $(<$($g:ident),+>)?) => {
        impl$(<$($g: ::serde::Serialize),+>)? ::serde::Serialize for $ty$(<$($g),+>)? {
            fn serialize<S: ::serde::Serializer>(
                &self,
                serializer: S,
            ) -> ::core::result::Result<S::Ok, S::Error> {
                $ty::serialize(self, serializer)
            }
        }

        impl<'de $($(, $g: ::serde::Deserialize<'de>)+)?> ::serde::Deserialize<'de>
            for $ty$(<$($g),+>)?
        {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> ::core::result::Result<Self, D::Error> {
                $ty::deserialize($crate::tolerant::Enum(deserializer))
            }
        }
    };
}

/// Десериализатор, у которого derive enum'а берёт только `deserialize_enum`.
pub struct Enum<D>(pub D);

impl<'de, D: Deserializer<'de>> Deserializer<'de> for Enum<D> {
    type Error = D::Error;

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, D::Error> {
        self.0.deserialize_any(Form { name, visitor })
    }

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, D::Error> {
        self.0.deserialize_any(visitor)
    }

    fn is_human_readable(&self) -> bool {
        self.0.is_human_readable()
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct identifier ignored_any
    }
}

/// Различает форму enum'а по тому, что лежит на проводе.
struct Form<V> {
    name: &'static str,
    visitor: V,
}

impl<'de, V: Visitor<'de>> Visitor<'de> for Form<V> {
    type Value = V::Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "enum {}: a variant name or a single-key map", self.name)
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<V::Value, E> {
        self.visitor.visit_enum(v.into_deserializer())
    }

    fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<V::Value, E> {
        self.visitor
            .visit_enum(BorrowedStrDeserializer::<E>::new(v))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<V::Value, E> {
        self.visitor.visit_enum(v.into_deserializer())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<V::Value, A::Error> {
        let Some(tag) = map.next_key::<String>()? else {
            return Err(de::Error::invalid_length(0, &self));
        };

        self.visitor.visit_enum(Tagged {
            name: self.name,
            tag,
            map,
        })
    }
}

/// `{tag: payload}`: тег уже прочитан, payload — следующее значение карты.
struct Tagged<A> {
    name: &'static str,
    tag: String,
    map: A,
}

impl<'de, A: MapAccess<'de>> EnumAccess<'de> for Tagged<A> {
    type Error = A::Error;
    type Variant = Payload<A>;

    fn variant_seed<T: DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> Result<(T::Value, Payload<A>), A::Error> {
        let variant = seed.deserialize(self.tag.into_deserializer())?;
        Ok((
            variant,
            Payload {
                name: self.name,
                map: self.map,
            },
        ))
    }
}

struct Payload<A> {
    name: &'static str,
    map: A,
}

impl<'de, A: MapAccess<'de>> Payload<A> {
    fn end<T>(mut self, value: T) -> Result<T, A::Error> {
        match self.map.next_key::<IgnoredAny>()? {
            None => Ok(value),
            Some(IgnoredAny) => Err(de::Error::custom(format_args!(
                "enum {}: a variant map must hold exactly one key",
                self.name
            ))),
        }
    }
}

impl<'de, A: MapAccess<'de>> VariantAccess<'de> for Payload<A> {
    type Error = A::Error;

    /// Сюда же приходит `Unknown`: payload незнакомого варианта пропускается.
    fn unit_variant(mut self) -> Result<(), A::Error> {
        self.map.next_value::<IgnoredAny>()?;
        self.end(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(
        mut self,
        seed: T,
    ) -> Result<T::Value, A::Error> {
        let value = self.map.next_value_seed(seed)?;
        self.end(value)
    }

    fn tuple_variant<V: Visitor<'de>>(
        mut self,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, A::Error> {
        let value = self.map.next_value_seed(Tuple { len, visitor })?;
        self.end(value)
    }

    fn struct_variant<V: Visitor<'de>>(
        mut self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, A::Error> {
        let value = self.map.next_value_seed(Struct {
            name: self.name,
            fields,
            visitor,
        })?;
        self.end(value)
    }
}

struct Tuple<V> {
    len: usize,
    visitor: V,
}

impl<'de, V: Visitor<'de>> DeserializeSeed<'de> for Tuple<V> {
    type Value = V::Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<V::Value, D::Error> {
        deserializer.deserialize_tuple(self.len, self.visitor)
    }
}

struct Struct<V> {
    name: &'static str,
    fields: &'static [&'static str],
    visitor: V,
}

impl<'de, V: Visitor<'de>> DeserializeSeed<'de> for Struct<V> {
    type Value = V::Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<V::Value, D::Error> {
        deserializer.deserialize_struct(self.name, self.fields, self.visitor)
    }
}
