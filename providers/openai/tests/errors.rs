//! Классификация ответов с не-2xx статусом.

use std::time::Duration;

use eva::time::HumanDuration;
use serde_json::json;

use eva_protocol::types::generate::ProviderErrorKind as Kind;
use eva_provider_openai::classify;

#[test]
fn classification_table() {
    let openrouter =
        |code: u16, message: &str| json!({"error": {"code": code, "message": message}}).to_string();
    let cases = [
        (408, openrouter(408, "Request timed out"), Kind::Transient),
        (429, openrouter(429, "Rate limit exceeded"), Kind::Transient),
        (500, "internal".to_owned(), Kind::Transient),
        (502, openrouter(502, "Provider returned error"), Kind::Transient),
        (503, openrouter(503, "No available provider"), Kind::Transient),
        (401, openrouter(401, "No auth credentials found"), Kind::Auth),
        (402, openrouter(402, "Insufficient credits"), Kind::Quota),
        (
            403,
            json!({"error": {"code": 403, "message": "flagged", "metadata": {
                "reasons": ["violence"], "flagged_input": "…", "provider_name": "OpenAI", "model_slug": "openai/gpt-5"
            }}})
            .to_string(),
            Kind::ContentFilter,
        ),
        (403, openrouter(403, "Key is disabled"), Kind::Auth),
        (404, openrouter(404, "No endpoints found for x/y"), Kind::NotFound),
        (
            400,
            openrouter(400, "This endpoint's maximum context length is 200000 tokens. However, you requested about 250000 tokens"),
            Kind::Overflow,
        ),
        (
            400,
            json!({"error": {"code": 400, "message": "Provider returned error", "metadata": {
                "raw": "{\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"prompt is too long: 210000 tokens > 200000 maximum\"}}",
                "provider_name": "Anthropic"
            }}})
            .to_string(),
            Kind::Overflow,
        ),
        (
            400,
            json!({"error": {"code": "context_length_exceeded", "message": "input too big", "type": "invalid_request_error"}}).to_string(),
            Kind::Overflow,
        ),
        (
            400,
            json!({"error": {"code": 400, "message": "the request exceeds the available context size", "type": "exceed_context_size_error"}}).to_string(),
            Kind::Overflow,
        ),
        (400, openrouter(400, "tool_use ids must be unique"), Kind::Invalid),
        (422, "not json at all".to_owned(), Kind::Invalid),
        (302, String::new(), Kind::Other),
    ];
    for (status, body, kind) in cases {
        let error = classify(status, None, &body);
        assert_eq!(error.kind, kind, "{status} {body}");
        assert_eq!(error.http_status, Some(status));
        assert_eq!(error.raw, body);
    }
}

#[test]
fn retry_after_only_where_it_helps() {
    let hint = Some(HumanDuration(Duration::from_secs(7)));
    assert_eq!(classify(429, hint, "").retry_after, hint);
    assert_eq!(classify(503, hint, "").retry_after, hint);
    assert_eq!(classify(402, hint, "").retry_after, hint);
    assert_eq!(classify(401, hint, "").retry_after, None);
    assert_eq!(classify(400, hint, "").retry_after, None);
}
