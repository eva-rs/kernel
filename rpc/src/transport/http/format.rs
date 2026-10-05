//! Кодек тела HTTP-моста: `Content-Type` выбирает кодек запроса, `Accept` —
//! ответа. msgpack здесь — тот же, что на проводе (`to_vec_named`, `bin`
//! для байтов); в JSON байты — массив чисел, остальное как у `serde_json`.

use serde::{Serialize, de::DeserializeOwned};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Msgpack,
}

impl Format {
    /// Значение `Content-Type`; параметры вроде `charset` не учитываются.
    pub fn from_mime_type(mime: &str) -> Option<Self> {
        Self::from_essence(essence(mime))
    }

    /// Самый предпочтительный поддерживаемый кодек из `Accept`.
    ///
    /// Качество кодека задаёт самый специфичный подходящий диапазон, так что
    /// `application/msgpack;q=0, */*` исключает msgpack. Ничья — в пользу
    /// `fallback` (кодека запроса), затем явно названного кодека; `None` —
    /// ничего подходящего.
    pub fn negotiate(accept: &str, fallback: Self) -> Option<Self> {
        const FORMATS: [Format; 2] = [Format::Json, Format::Msgpack];
        // (специфичность, качество) самого специфичного диапазона на кодек.
        let mut matched: [Option<(u8, f32)>; 2] = [None; 2];

        for range in accept.split(',') {
            let mut parts = range.split(';');
            let essence = parts.next().unwrap_or_default().trim();
            let quality = parts
                .filter_map(|param| {
                    let (name, value) = param.split_once('=')?;
                    name.trim()
                        .eq_ignore_ascii_case("q")
                        .then(|| value.trim().parse::<f32>().ok())
                        .flatten()
                })
                .next()
                .unwrap_or(1.0);
            let quality = if quality.is_nan() { 0.0 } else { quality };

            let (specificity, only) = if essence == "*/*" {
                (0, None)
            } else if essence.eq_ignore_ascii_case("application/*") {
                (1, None)
            } else if let Some(format) = Self::from_essence(essence) {
                (2, Some(format))
            } else {
                continue;
            };

            for (slot, format) in matched.iter_mut().zip(FORMATS) {
                if only.is_some_and(|only| only != format) {
                    continue;
                }
                *slot = match *slot {
                    Some((s, q)) if s > specificity => Some((s, q)),
                    Some((s, q)) if s == specificity => Some((s, q.max(quality))),
                    _ => Some((specificity, quality)),
                };
            }
        }

        matched
            .into_iter()
            .zip(FORMATS)
            .filter_map(|(slot, format)| {
                let (specificity, quality) = slot?;
                (quality > 0.0).then_some(((quality, format == fallback, specificity), format))
            })
            .max_by(|(a, _), (b, _)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(_, format)| format)
    }

    pub const fn mime_type(self) -> &'static str {
        match self {
            Self::Json => "application/json",
            Self::Msgpack => "application/msgpack",
        }
    }

    fn from_essence(essence: &str) -> Option<Self> {
        const MSGPACK: [&str; 3] = [
            "application/msgpack",
            "application/x-msgpack",
            "application/vnd.msgpack",
        ];

        if essence.eq_ignore_ascii_case("application/json") {
            Some(Self::Json)
        } else if MSGPACK.iter().any(|m| essence.eq_ignore_ascii_case(m)) {
            Some(Self::Msgpack)
        } else {
            None
        }
    }

    pub fn dump<T: Serialize + ?Sized>(self, what: &T) -> Result<Vec<u8>, String> {
        match self {
            Self::Json => serde_json::to_vec(what).map_err(|e| e.to_string()),
            Self::Msgpack => rmp_serde::to_vec_named(what).map_err(|e| e.to_string()),
        }
    }

    pub fn load<T: DeserializeOwned>(self, raw: &[u8]) -> Result<T, String> {
        match self {
            Self::Json => serde_json::from_slice(raw).map_err(|e| e.to_string()),
            Self::Msgpack => rmp_serde::from_slice(raw).map_err(|e| e.to_string()),
        }
    }
}

fn essence(mime: &str) -> &str {
    mime.split(';').next().unwrap_or_default().trim()
}

#[cfg(test)]
mod tests {
    use super::Format::{self, *};

    #[test]
    fn content_type_ignores_parameters() {
        assert_eq!(
            Format::from_mime_type("application/json; charset=utf-8"),
            Some(Json)
        );
        assert_eq!(Format::from_mime_type("Application/MsgPack"), Some(Msgpack));
        assert_eq!(
            Format::from_mime_type("application/x-msgpack"),
            Some(Msgpack)
        );
        assert_eq!(Format::from_mime_type("*/*"), None);
        assert_eq!(Format::from_mime_type("text/plain"), None);
    }

    #[test]
    fn accept_negotiation() {
        let n = |accept| Format::negotiate(accept, Json);

        assert_eq!(n("application/json, text/plain, */*"), Some(Json));
        assert_eq!(n("application/msgpack"), Some(Msgpack));
        assert_eq!(
            n("application/json;q=0.5, application/msgpack"),
            Some(Msgpack)
        );
        assert_eq!(n("*/*, application/msgpack"), Some(Json));
        assert_eq!(n("*/*;q=0.9, application/msgpack"), Some(Msgpack));
        assert_eq!(n("application/json, application/msgpack"), Some(Json));
        assert_eq!(
            Format::negotiate("application/json, text/plain, */*", Msgpack),
            Some(Msgpack)
        );
        assert_eq!(n("application/msgpack;q=0, */*;q=0.1"), Some(Json));
        assert_eq!(Format::negotiate("*/*", Msgpack), Some(Msgpack));
        assert_eq!(
            Format::negotiate("application/msgpack;q=0, */*", Msgpack),
            Some(Json)
        );
        assert_eq!(n("application/json;q=0, */*"), Some(Msgpack));
        assert_eq!(
            n("*/*, application/json;q=0, application/msgpack;q=0"),
            None
        );
        assert_eq!(n("text/html"), None);
    }
}
