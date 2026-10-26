//! Сырые байты: картинки, подписи thinking, чанки blob'ов, файлы модуля, payload кадров.

use std::{borrow::Cow, fmt, ops::Deref};

use eva::{
    _priv::schemars::{JsonSchema, Schema, SchemaGenerator, json_schema},
    data,
};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, Visitor},
};

/// В msgpack — `bin` (не массив чисел), в JSON — base64-строка: так же её
/// описывает схема.
#[data(not(serde, JsonSchema, Debug))]
#[derive(Default, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct Bytes(pub Vec<u8>);

impl Bytes {
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}

impl Deref for Bytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(value: Vec<u8>) -> Self {
        Self(value)
    }
}

impl From<&[u8]> for Bytes {
    fn from(value: &[u8]) -> Self {
        Self(value.to_vec())
    }
}

impl<const N: usize> From<[u8; N]> for Bytes {
    fn from(value: [u8; N]) -> Self {
        Self(value.to_vec())
    }
}

impl From<Bytes> for Vec<u8> {
    fn from(value: Bytes) -> Self {
        value.0
    }
}

/// Без копии, если буфер уникален.
impl From<eva::bytes::Bytes> for Bytes {
    fn from(value: eva::bytes::Bytes) -> Self {
        Self(Vec::from(value))
    }
}

impl From<Bytes> for eva::bytes::Bytes {
    fn from(value: Bytes) -> Self {
        Self::from(value.0)
    }
}

/// Длина и начало: мегабайтный чанк в логе не нужен целиком.
impl fmt::Debug for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const SHOWN: usize = 16;

        write!(f, "Bytes[{}: ", self.0.len())?;
        for byte in self.0.iter().take(SHOWN) {
            write!(f, "{byte:02x}")?;
        }
        if self.0.len() > SHOWN {
            f.write_str("…")?;
        }
        f.write_str("]")
    }
}

impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.collect_str(&Base64(&self.0))
        } else {
            serde_bytes::Bytes::new(&self.0).serialize(serializer)
        }
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // В `Content` (untagged, flatten) флаг врёт `true` при msgpack-байтах внутри, поэтому визитор берёт любую форму.
        if deserializer.is_human_readable() {
            deserializer.deserialize_any(BytesVisitor)
        } else {
            deserializer.deserialize_byte_buf(BytesVisitor)
        }
    }
}

struct BytesVisitor;

impl<'de> Visitor<'de> for BytesVisitor {
    type Value = Bytes;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bytes or a base64 string")
    }

    fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Bytes, E> {
        Ok(Bytes(v.to_vec()))
    }

    fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Bytes, E> {
        Ok(Bytes(v))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Bytes, E> {
        decode_base64(v)
            .map(Bytes)
            .ok_or_else(|| E::invalid_value(de::Unexpected::Str(v), &"base64"))
    }
}

impl JsonSchema for Bytes {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Bytes")
    }

    fn schema_id() -> Cow<'static, str> {
        Cow::Borrowed(concat!(module_path!(), "::Bytes"))
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "contentEncoding": "base64",
            "description": "Raw bytes: msgpack `bin` on the wire, standard padded base64 in JSON",
        })
    }
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Стандартный base64 с паддингом.
struct Base64<'a>(&'a [u8]);

impl fmt::Display for Base64<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for chunk in self.0.chunks(3) {
            let group = chunk
                .iter()
                .enumerate()
                .fold(0_u32, |acc, (i, &b)| acc | u32::from(b) << (16 - 8 * i));
            for i in 0..4 {
                let c = if i <= chunk.len() {
                    ALPHABET[(group >> (18 - 6 * i) & 0x3f) as usize]
                } else {
                    b'='
                };
                fmt::Write::write_char(f, char::from(c))?;
            }
        }
        Ok(())
    }
}

/// Паддинг необязателен; `None` — чужой символ или обрубок в один знак.
fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let body = s
        .strip_suffix("==")
        .or_else(|| s.strip_suffix('='))
        .unwrap_or(s)
        .as_bytes();
    if body.len() % 4 == 1 {
        return None;
    }

    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    let (mut acc, mut bits) = (0_u32, 0_u32);
    for &c in body {
        let sextet = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = acc << 6 | u32::from(sextet);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_rfc4648_vectors() {
        let vectors: [(&[u8], &str); 7] = [
            (b"", ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ];
        for (raw, encoded) in vectors {
            assert_eq!(Base64(raw).to_string(), encoded);
            assert_eq!(decode_base64(encoded).as_deref(), Some(raw));
        }
        assert_eq!(decode_base64("Zm9vYg").as_deref(), Some(&b"foob"[..]));
        assert_eq!(decode_base64("Z"), None);
        assert_eq!(decode_base64("Zm9v!"), None);
    }
}
