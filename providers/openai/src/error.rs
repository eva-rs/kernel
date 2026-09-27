//! Ответы вендора → `ProviderError`: вид решает HTTP-статус (или `error.code`
//! внутри стрима), текст смотрится только у 400/403, где статус неоднозначен.

use std::time::Duration;

use eva::time::HumanDuration;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde_json::Value;

use eva_protocol::types::generate::{ProviderError, ProviderErrorKind as Kind, RateLimits};

/// Ответ с не-2xx статусом.
pub fn classify(status: u16, retry_after: Option<HumanDuration>, body: &str) -> ProviderError {
    let error = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").cloned())
        .unwrap_or(Value::Null);
    let kind = kind(status, &error);
    ProviderError {
        kind,
        http_status: Some(status),
        raw: body.to_owned(),
        retry_after: retry_after.filter(|_| matches!(kind, Kind::Transient | Kind::Quota)),
    }
}

/// Кадр `{"error": {...}}` посреди стрима: HTTP уже ответил 200, вид — по `error.code`.
pub(crate) fn in_stream(error: &Value, raw: &str) -> ProviderError {
    let kind = match error.get("code").and_then(Value::as_u64) {
        Some(code) => kind(u16::try_from(code).unwrap_or(0), error),
        None if overflow(error) => Kind::Overflow,
        None => Kind::Other,
    };
    ProviderError {
        kind,
        http_status: None,
        raw: raw.to_owned(),
        retry_after: None,
    }
}

fn kind(status: u16, error: &Value) -> Kind {
    match status {
        408 | 429 | 500..=599 => Kind::Transient,
        401 => Kind::Auth,
        402 => Kind::Quota,
        // 403 у OpenRouter — и модерация, и нехватка прав ключа; модерацию выдают её метаданные.
        403 if moderation(error) => Kind::ContentFilter,
        403 => Kind::Auth,
        404 => Kind::NotFound,
        400..=499 if overflow(error) => Kind::Overflow,
        400..=499 => Kind::Invalid,
        _ => Kind::Other,
    }
}

fn moderation(error: &Value) -> bool {
    let metadata = &error["metadata"];
    metadata.get("reasons").is_some() || metadata.get("flagged_input").is_some()
}

/// Вход + `max_tokens` больше окна: формулировки OpenAI, Anthropic, vLLM, llama-server.
fn overflow(error: &Value) -> bool {
    const MARKERS: [&str; 7] = [
        "context_length_exceeded",
        "context length",
        "context limit",
        "context window",
        "context size",
        "prompt is too long",
        "prompt too long",
    ];
    let text = |v: &Value| v.as_str().unwrap_or_default().to_lowercase();
    // OpenRouter кладёт ответ апстрима в `metadata.raw`, а в `message` — «Provider returned error».
    let texts = [text(&error["message"]), text(&error["metadata"]["raw"])];
    error["code"].as_str() == Some("context_length_exceeded")
        || texts.iter().any(|t| MARKERS.iter().any(|m| t.contains(m)))
}

/// Сбой до ответа: сеть и таймауты переживаются повтором, битый запрос — нет.
pub(crate) fn transport(error: &reqwest::Error) -> ProviderError {
    ProviderError {
        kind: if error.is_builder() {
            Kind::Invalid
        } else {
            Kind::Transient
        },
        http_status: None,
        raw: error.to_string(),
        retry_after: None,
    }
}

pub(crate) fn invalid(raw: impl Into<String>) -> ProviderError {
    ProviderError {
        kind: Kind::Invalid,
        http_status: None,
        raw: raw.into(),
        retry_after: None,
    }
}

pub(crate) fn other(raw: impl Into<String>) -> ProviderError {
    ProviderError {
        kind: Kind::Other,
        http_status: None,
        raw: raw.into(),
        retry_after: None,
    }
}

/// Подсказка приходит из сети: абсурдное значение не должно парковать повторы.
const RETRY_AFTER_MAX: Duration = Duration::from_secs(3600);

/// Только секунды: HTTP-дату в `Retry-After` вендоры инференса не шлют.
pub(crate) fn retry_after(headers: &HeaderMap) -> Option<HumanDuration> {
    let secs = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|s| !s.is_nan())?;
    Duration::try_from_secs_f64(secs.min(RETRY_AFTER_MAX.as_secs_f64()))
        .ok()
        .map(HumanDuration)
}

/// Заголовки OpenAI; OpenRouter их не шлёт — тогда `None`.
pub(crate) fn rate_limits(headers: &HeaderMap) -> Option<RateLimits> {
    let number = |name: &str| headers.get(name)?.to_str().ok()?.trim().parse::<u64>().ok();
    let requests = number("x-ratelimit-remaining-requests");
    let tokens = number("x-ratelimit-remaining-tokens");
    (requests.is_some() || tokens.is_some()).then_some(RateLimits {
        requests_remaining: requests,
        tokens_remaining: tokens,
        reset_at: None,
    })
}
