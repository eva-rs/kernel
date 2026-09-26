//! Фикстуры SSE → точные последовательности `GenItem`, через настоящий SSE-парсер.

use std::convert::Infallible;

use futures::{StreamExt, channel::mpsc, stream};
use serde_json::{Value, json};

use eva_core::GenSink;
use eva_protocol::types::{
    Bytes, ModelId,
    generate::{CacheMode, GenItem, ProviderError, ProviderErrorKind, Stop, StopReason, Usage},
};
use eva_provider_openai::{Decoder, pump};

const MODEL: &str = "anthropic/claude-sonnet-4.5";

fn sse(chunks: &[Value]) -> String {
    chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect()
}

fn done() -> &'static str {
    "data: [DONE]\n\n"
}

fn delta(delta: Value) -> Value {
    json!({"id": "gen-1", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": delta, "finish_reason": null}]})
}

fn finish(reason: &str) -> Value {
    json!({"id": "gen-1", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
}

fn usage(usage: Value) -> Value {
    json!({"id": "gen-1", "object": "chat.completion.chunk", "choices": [], "usage": usage})
}

/// Тело режется по `split` байт — и посреди UTF-8, и посреди строки `data:`.
async fn run_split(
    body: &str,
    cache: CacheMode,
    split: usize,
) -> (Vec<GenItem>, Result<(), ProviderError>) {
    let bytes: Vec<Result<Vec<u8>, Infallible>> = body
        .as_bytes()
        .chunks(split)
        .map(|c| Ok(c.to_vec()))
        .collect();
    let (tx, rx) = mpsc::unbounded();
    let mut sink = GenSink::new(tx);
    let result = pump(
        stream::iter(bytes),
        Decoder::new(ModelId::from(MODEL), cache),
        &mut sink,
    )
    .await;
    drop(sink);
    (rx.collect().await, result)
}

async fn run(body: &str) -> (Vec<GenItem>, Result<(), ProviderError>) {
    let whole = run_split(body, CacheMode::Short, body.len().max(1)).await;
    for split in [1, 3, 7] {
        let (items, result) = run_split(body, CacheMode::Short, split).await;
        assert_eq!(items, whole.0, "split every {split} bytes");
        assert_eq!(result, whole.1, "split every {split} bytes");
    }
    whole
}

fn text(text: &str) -> GenItem {
    GenItem::TextDelta { text: text.into() }
}

fn thinking(text: &str) -> GenItem {
    GenItem::ThinkingDelta { text: text.into() }
}

fn stop(reason: StopReason) -> GenItem {
    GenItem::Stop(Stop {
        reason,
        details: None,
    })
}

fn blob(value: &Value) -> Bytes {
    Bytes(serde_json::to_vec(value).unwrap())
}

#[tokio::test]
async fn plain_text() {
    // Комментарий SSE — keep-alive OpenRouter, событием не считается.
    let body = ": OPENROUTER PROCESSING\n\n".to_owned()
        + &sse(&[
            delta(json!({"role": "assistant", "content": ""})),
            delta(json!({"content": "Привет, "})),
            delta(json!({"content": "Господин"})),
            finish("stop"),
        ])
        + done();
    let (items, result) = run(&body).await;
    result.unwrap();
    assert_eq!(
        items,
        [
            text("Привет, "),
            text("Господин"),
            stop(StopReason::EndTurn)
        ]
    );
}

#[tokio::test]
async fn reasoning_details_become_opaque_blocks() {
    let body = sse(&[
        delta(json!({
            "reasoning": "Let me",
            "reasoning_details": [{"type": "reasoning.text", "text": "Let me", "index": 0, "format": "anthropic-claude-v1"}]
        })),
        delta(json!({
            "reasoning": " think",
            "reasoning_details": [{"type": "reasoning.text", "text": " think", "index": 0, "format": "anthropic-claude-v1"}]
        })),
        delta(json!({
            "reasoning_details": [{"type": "reasoning.text", "signature": "EqQBsig==", "index": 0, "format": "anthropic-claude-v1"}]
        })),
        delta(json!({
            "reasoning_details": [{"type": "reasoning.encrypted", "data": "gAAAA", "index": 1, "format": "anthropic-claude-v1"}]
        })),
        delta(json!({"content": "Answer"})),
        finish("stop"),
    ]) + done();
    let (items, result) = run(&body).await;
    result.unwrap();
    let model = ModelId::from(MODEL);
    assert_eq!(
        items,
        [
            thinking("Let me"),
            thinking(" think"),
            GenItem::ThinkingBlock {
                signature: blob(&json!({
                    "type": "reasoning.text",
                    "text": "Let me think",
                    "signature": "EqQBsig==",
                    "index": 0,
                    "format": "anthropic-claude-v1",
                })),
                model: model.clone(),
            },
            GenItem::RedactedThinking {
                data: blob(
                    &json!({"type": "reasoning.encrypted", "data": "gAAAA", "index": 1, "format": "anthropic-claude-v1"})
                ),
                model,
            },
            text("Answer"),
            stop(StopReason::EndTurn),
        ]
    );
}

#[tokio::test]
async fn summary_details_and_plain_reasoning_fields() {
    let body = sse(&[
        delta(
            json!({"reasoning_details": [{"type": "reasoning.summary", "summary": "Plan", "index": 0, "format": "openai-responses-v1"}]}),
        ),
        delta(
            json!({"reasoning_details": [{"type": "reasoning.summary", "summary": " it", "index": 0, "format": "openai-responses-v1"}]}),
        ),
    ]) + &sse(&[delta(json!({"content": "Done"})), finish("stop")])
        + done();
    let (items, result) = run(&body).await;
    result.unwrap();
    assert_eq!(
        items,
        [
            thinking("Plan"),
            thinking(" it"),
            GenItem::ThinkingBlock {
                signature: blob(
                    &json!({"type": "reasoning.summary", "summary": "Plan it", "index": 0, "format": "openai-responses-v1"})
                ),
                model: ModelId::from(MODEL),
            },
            text("Done"),
            stop(StopReason::EndTurn),
        ]
    );

    let vllm = sse(&[
        delta(json!({"reasoning_content": "hmm"})),
        delta(json!({"content": "ok"})),
        finish("stop"),
    ]);
    let (items, result) = run(&vllm).await;
    result.unwrap();
    assert_eq!(
        items,
        [thinking("hmm"), text("ok"), stop(StopReason::EndTurn)]
    );
}

#[tokio::test]
async fn inline_think_tags_split_across_chunks() {
    let body = sse(&[
        delta(json!({"content": "<thi"})),
        delta(json!({"content": "nk>hmm</th"})),
        delta(json!({"content": "ink>ok <"})),
        delta(json!({"content": "b>"})),
        finish("stop"),
    ]) + done();
    let (items, result) = run(&body).await;
    result.unwrap();
    assert_eq!(
        items,
        [
            thinking("hmm"),
            text("ok "),
            text("<b>"),
            stop(StopReason::EndTurn)
        ]
    );

    let held = sse(&[delta(json!({"content": "a<thi"})), finish("length")]);
    let (items, result) = run(&held).await;
    result.unwrap();
    assert_eq!(
        items,
        [text("a"), text("<thi"), stop(StopReason::MaxTokens)]
    );
}

#[tokio::test]
async fn tool_calls_assembled_by_index() {
    let body = sse(&[
        delta(
            json!({"tool_calls": [{"index": 0, "id": "call_a", "type": "function", "function": {"name": "fs_read", "arguments": "{\"pa"}}]}),
        ),
        delta(
            json!({"tool_calls": [{"index": 1, "id": "", "type": "function", "function": {"name": "shell", "arguments": "{bro"}}]}),
        ),
        delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": "th\": \"a.txt\"}"}}]})),
        delta(json!({"tool_calls": [{"index": 1, "function": {"arguments": "ken"}}]})),
        delta(
            json!({"tool_calls": [{"index": 2, "id": "call_c", "function": {"name": "noop", "arguments": ""}}]}),
        ),
        finish("tool_calls"),
    ]) + done();
    let (items, result) = run(&body).await;
    result.unwrap();
    assert_eq!(
        items,
        [
            GenItem::ToolUse {
                id: "call_a".into(),
                name: "fs_read".into(),
                input: json!({"path": "a.txt"}),
            },
            GenItem::ToolUse {
                id: "call_1".into(),
                name: "shell".into(),
                input: Value::String("{broken".into()),
            },
            GenItem::ToolUse {
                id: "call_c".into(),
                name: "noop".into(),
                input: json!({}),
            },
            stop(StopReason::ToolUse),
        ]
    );
}

#[tokio::test]
async fn finish_before_usage_with_cache_and_reasoning() {
    let body = sse(&[
        delta(json!({"content": "hi"})),
        finish("stop"),
        usage(json!({
            "prompt_tokens": 1000,
            "completion_tokens": 200,
            "total_tokens": 1200,
            "cost": 0.0123,
            "prompt_tokens_details": {"cached_tokens": 600, "cache_write_tokens": 300},
            "completion_tokens_details": {"reasoning_tokens": 50}
        })),
    ]) + done();
    let expected = |cache_write_short, cache_write_long| {
        vec![
            text("hi"),
            GenItem::Usage(Usage {
                input: 100,
                cache_read: 600,
                cache_write_short,
                cache_write_long,
                output: 200,
                reasoning: 50,
                cost: Some(0.0123),
                ..Usage::default()
            }),
            stop(StopReason::EndTurn),
        ]
    };
    let (items, result) = run(&body).await;
    result.unwrap();
    assert_eq!(items, expected(300, 0));

    let (items, result) = run_split(&body, CacheMode::Long, 5).await;
    result.unwrap();
    assert_eq!(items, expected(0, 300));
}

#[tokio::test]
async fn usage_without_details_and_rate_limits() {
    let body = sse(&[
        finish("stop"),
        usage(json!({"prompt_tokens": 12, "completion_tokens": 3})),
    ]);
    let (tx, rx) = mpsc::unbounded();
    let mut sink = GenSink::new(tx);
    let limits = eva_protocol::types::generate::RateLimits {
        requests_remaining: Some(99),
        tokens_remaining: None,
        reset_at: None,
    };
    let decoder = Decoder::new(ModelId::from(MODEL), CacheMode::Off).with_rate_limits(Some(limits));
    pump(
        stream::iter([Ok::<_, Infallible>(body.into_bytes())]),
        decoder,
        &mut sink,
    )
    .await
    .unwrap();
    drop(sink);
    let items: Vec<GenItem> = rx.collect().await;
    assert_eq!(
        items,
        [
            GenItem::Usage(Usage {
                input: 12,
                output: 3,
                rate_limits: Some(limits),
                ..Usage::default()
            }),
            stop(StopReason::EndTurn),
        ]
    );
}

#[tokio::test]
async fn stop_reasons() {
    for (reason, expected) in [
        ("content_filter", StopReason::Refusal),
        ("length", StopReason::MaxTokens),
        ("function_call", StopReason::ToolUse),
    ] {
        let (items, result) = run(&sse(&[finish(reason)])).await;
        result.unwrap();
        assert_eq!(items, [stop(expected)], "{reason}");
    }
    let (items, _) = run(&sse(&[finish("guardrail")])).await;
    assert_eq!(
        items,
        [GenItem::Stop(Stop {
            reason: StopReason::Other,
            details: Some("guardrail".into()),
        })]
    );

    let (items, result) = run(&(sse(&[delta(json!({"content": "x"}))]) + done())).await;
    result.unwrap();
    assert_eq!(
        items,
        [text("x"), stop(StopReason::EndTurn)],
        "[DONE] closes without finish_reason"
    );
}

#[tokio::test]
async fn mid_stream_error_chunk() {
    let body = sse(&[
        delta(json!({"content": "partial"})),
        json!({
            "id": "gen-1",
            "object": "chat.completion.chunk",
            "provider": "Anthropic",
            "error": {"code": 502, "message": "Upstream error", "metadata": {"error_type": "provider_unavailable"}},
            "choices": [{"index": 0, "delta": {"content": ""}, "finish_reason": "error"}]
        }),
    ]) + done();
    let (items, result) = run(&body).await;
    assert_eq!(items, [text("partial")]);
    let error = result.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Transient);
    assert_eq!(error.http_status, None);
    assert!(error.raw.contains("Upstream error"));

    let overflow =
        sse(&[json!({"error": {"code": "context_length_exceeded", "message": "too long"}})]);
    let (_, result) = run(&overflow).await;
    assert_eq!(result.unwrap_err().kind, ProviderErrorKind::Overflow);
}

#[tokio::test]
async fn cut_stream_is_other() {
    let body = sse(&[delta(json!({"content": "partial"}))]);
    let (items, result) = run(&body).await;
    assert_eq!(items, [text("partial")]);
    assert_eq!(result.unwrap_err().kind, ProviderErrorKind::Other);
}

#[tokio::test]
async fn generated_images() {
    let body = sse(&[
        delta(json!({"content": "Here:"})),
        delta(json!({"images": [
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}},
            {"type": "image_url", "image_url": {"url": "https://example.invalid/not-inline.png"}}
        ]})),
        finish("stop"),
    ]) + done();
    let (items, result) = run(&body).await;
    result.unwrap();
    assert_eq!(
        items,
        [
            text("Here:"),
            GenItem::Image {
                media_type: "image/png".into(),
                data: Bytes(vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]),
            },
            stop(StopReason::EndTurn),
        ]
    );
}

#[tokio::test]
async fn gone_consumer_ends_quietly() {
    let body = sse(&[delta(json!({"content": "a"})), finish("stop")]);
    let (tx, rx) = mpsc::unbounded::<GenItem>();
    drop(rx);
    let mut sink = GenSink::new(tx);
    let result = pump(
        stream::iter([Ok::<_, Infallible>(body.into_bytes())]),
        Decoder::new(ModelId::from(MODEL), CacheMode::Off),
        &mut sink,
    )
    .await;
    assert_eq!(result, Ok(()));
}

#[tokio::test]
async fn refusal_field_and_unindexed_signature_chunk() {
    let body = sse(&[
        delta(json!({"reasoning_details": [{"type": "reasoning.text", "text": "hm", "index": 0}]})),
        delta(json!({"reasoning_details": [{"type": "reasoning.text", "signature": "sig"}]})),
        delta(json!({"refusal": "I can't help with that."})),
        finish("stop"),
    ]);
    let (items, result) = run(&body).await;
    result.unwrap();
    assert_eq!(
        items,
        [
            thinking("hm"),
            GenItem::ThinkingBlock {
                signature: blob(
                    &json!({"type": "reasoning.text", "text": "hm", "index": 0, "signature": "sig"})
                ),
                model: ModelId::from(MODEL),
            },
            text("I can't help with that."),
            stop(StopReason::Refusal),
        ]
    );
}
