//! SSE `chat.completion.chunk` → `GenItem`; `finish_reason` приходит раньше
//! usage-чанка, поэтому поток финализируется на `[DONE]` или конце тела.

use std::{fmt, pin::pin};

use eventsource_stream::Eventsource;
use futures::{Stream, StreamExt};
use serde_json::{Map, Value};

use eva_core::GenSink;
use eva_protocol::types::{
    Bytes, ModelId,
    generate::{CacheMode, GenItem, ProviderError, RateLimits, Stop, StopReason, Usage},
};

use crate::{
    error::{in_stream, other},
    request::parse_data_url,
};

/// Разбор одного ответа; чистый, без I/O — тесты кормят его фикстурами.
#[derive(Debug)]
pub struct Decoder {
    model: ModelId,
    cache: CacheMode,
    rate_limits: Option<RateLimits>,
    think: ThinkSplit,
    /// Открытый элемент `reasoning_details`: куски одного элемента склеиваются по `type` и `index`.
    reasoning: Option<Map<String, Value>>,
    tools: Vec<PendingTool>,
    stop: Option<Stop>,
    usage: Option<Usage>,
    /// OpenAI шлёт отказ полем `delta.refusal` при `finish_reason: stop`.
    refused: bool,
    done: bool,
}

impl Decoder {
    /// `model` — модель запроса: ею помечается thinking для реплея; `cache` делит запись кэша на TTL.
    pub fn new(model: ModelId, cache: CacheMode) -> Self {
        Self {
            model,
            cache,
            rate_limits: None,
            think: ThinkSplit::default(),
            reasoning: None,
            tools: Vec::new(),
            stop: None,
            usage: None,
            refused: false,
            done: false,
        }
    }

    pub fn with_rate_limits(mut self, rate_limits: Option<RateLimits>) -> Self {
        self.rate_limits = rate_limits;
        self
    }

    /// Поле `data` одного события SSE.
    pub fn feed(&mut self, data: &str) -> Result<Vec<GenItem>, ProviderError> {
        let data = data.trim();
        let mut out = Vec::new();
        if data.is_empty() {
            return Ok(out);
        }
        if data == "[DONE]" {
            self.done = true;
            return Ok(out);
        }
        let chunk: Value = match serde_json::from_str(data) {
            Ok(chunk) => chunk,
            Err(e) => {
                log::warn!(error:% = e; "unparseable sse chunk skipped");
                return Ok(out);
            }
        };
        if let Some(error) = chunk.get("error").filter(|e| !e.is_null()) {
            return Err(in_stream(error, data));
        }
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(usage_of(usage, self.cache));
        }
        let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) else {
            return Ok(out);
        };
        let delta = &choice["delta"];

        // `delta.reasoning` у OpenRouter дублирует текст `reasoning_details`.
        match delta["reasoning_details"]
            .as_array()
            .filter(|d| !d.is_empty())
        {
            Some(details) => details
                .iter()
                .for_each(|d| self.reasoning_detail(d, &mut out)),
            None => {
                let thought = ["reasoning", "reasoning_content"]
                    .iter()
                    .find_map(|field| delta[*field].as_str().filter(|t| !t.is_empty()));
                if let Some(text) = thought {
                    out.push(GenItem::ThinkingDelta { text: text.into() });
                }
            }
        }

        if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            self.flush_reasoning(&mut out);
            self.think.push(text, &mut out);
        }

        if let Some(text) = delta["refusal"].as_str().filter(|t| !t.is_empty()) {
            self.flush_reasoning(&mut out);
            self.refused = true;
            out.push(GenItem::TextDelta { text: text.into() });
        }

        for image in delta["images"].as_array().into_iter().flatten() {
            self.flush_reasoning(&mut out);
            match parse_data_url(image["image_url"]["url"].as_str().unwrap_or_default()) {
                Ok((media_type, data)) => out.push(GenItem::Image { media_type, data }),
                // Картинка потеряна, текст ценнее обрыва.
                Err(e) => log::warn!(error:% = e; "generated image skipped"),
            }
        }

        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            self.flush_reasoning(&mut out);
            self.tool_call(call);
        }

        if let Some(reason) = choice["finish_reason"].as_str() {
            self.stop = Some(stop(reason));
        }
        Ok(out)
    }

    /// Пришёл `[DONE]`: дальше читать нечего.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Конец тела: хвосты, тулы, usage и `Stop`.
    pub fn finish(mut self) -> Result<Vec<GenItem>, ProviderError> {
        if self.stop.is_none() && !self.done {
            return Err(other("stream ended without finish_reason"));
        }
        let mut out = Vec::new();
        self.think.finish(&mut out);
        self.flush_reasoning(&mut out);
        let called = !self.tools.is_empty();
        out.extend(self.tools.into_iter().map(PendingTool::into_item));
        if let Some(mut usage) = self.usage {
            usage.rate_limits = self.rate_limits;
            out.push(GenItem::Usage(usage));
        }
        let mut stop = self.stop.unwrap_or(Stop {
            reason: if called {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            },
            details: None,
        });
        if self.refused && stop.reason == StopReason::EndTurn {
            stop.reason = StopReason::Refusal;
        }
        out.push(GenItem::Stop(stop));
        Ok(out)
    }

    fn reasoning_detail(&mut self, detail: &Value, out: &mut Vec<GenItem>) {
        let Some(detail) = detail.as_object() else {
            return;
        };
        // Кусок без `index` продолжает открытый элемент того же типа.
        let continues = self.reasoning.as_ref().is_some_and(|open| {
            open.get("type") == detail.get("type")
                && match (open.get("index"), detail.get("index")) {
                    (Some(have), Some(more)) => have == more,
                    _ => true,
                }
        });
        if continues {
            if let Some(open) = self.reasoning.as_mut() {
                merge(open, detail);
            }
        } else {
            self.flush_reasoning(out);
            self.reasoning = Some(detail.clone());
        }
        let text = detail
            .get("text")
            .or_else(|| detail.get("summary"))
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty());
        if let Some(text) = text {
            out.push(GenItem::ThinkingDelta { text: text.into() });
        }
    }

    /// Элемент уходит ядру целиком JSON'ом: подпись и шифротекст реплеятся байт-в-байт.
    fn flush_reasoning(&mut self, out: &mut Vec<GenItem>) {
        let Some(detail) = self.reasoning.take() else {
            return;
        };
        let encrypted = detail.get("type").and_then(Value::as_str) == Some("reasoning.encrypted");
        let blob = Bytes(serde_json::to_vec(&detail).unwrap_or_default());
        let model = self.model.clone();
        out.push(if encrypted {
            GenItem::RedactedThinking { data: blob, model }
        } else {
            GenItem::ThinkingBlock {
                signature: blob,
                model,
            }
        });
    }

    fn tool_call(&mut self, call: &Value) {
        let id = call["id"].as_str().filter(|id| !id.is_empty());
        let index = match (call["index"].as_u64(), self.tools.last(), id) {
            (Some(index), ..) => index,
            // Без `index` новый вызов узнаётся по новому `id`.
            (None, Some(last), Some(id)) if !last.id.is_empty() && last.id != id => last.index + 1,
            (None, Some(last), _) => last.index,
            (None, None, _) => 0,
        };
        let slot = match self.tools.iter().position(|t| t.index == index) {
            Some(slot) => slot,
            None => {
                self.tools.push(PendingTool {
                    index,
                    ..PendingTool::default()
                });
                self.tools.len() - 1
            }
        };
        let tool = &mut self.tools[slot];
        if let Some(id) = id {
            tool.id = id.into();
        }
        if let Some(name) = call["function"]["name"].as_str().filter(|n| !n.is_empty()) {
            tool.name = name.into();
        }
        match &call["function"]["arguments"] {
            Value::String(fragment) => tool.arguments.push_str(fragment),
            Value::Object(_) => tool.arguments = call["function"]["arguments"].to_string(),
            _ => {}
        }
    }
}

/// Тело ответа → `sink`; потребитель ушёл — генерация брошена без ошибки.
pub async fn pump<S, B, E>(
    body: S,
    mut decoder: Decoder,
    sink: &mut GenSink,
) -> Result<(), ProviderError>
where
    S: Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
    E: fmt::Display,
{
    let mut events = pin!(body.eventsource());
    while let Some(event) = events.next().await {
        let event = match event {
            Ok(event) => event,
            Err(e) if decoder.stop.is_some() => {
                log::warn!(error:% = e; "sse stream broke after finish_reason");
                break;
            }
            Err(e) => return Err(other(format!("sse stream: {e}"))),
        };
        for item in decoder.feed(&event.data)? {
            if sink.push(item).await.is_err() {
                return Ok(());
            }
        }
        if decoder.is_done() {
            break;
        }
    }
    for item in decoder.finish()? {
        if sink.push(item).await.is_err() {
            return Ok(());
        }
    }
    Ok(())
}

/// Строки склеиваются, прочие поля берутся из первого куска, где они есть.
fn merge(open: &mut Map<String, Value>, more: &Map<String, Value>) {
    for (key, value) in more {
        match (open.get_mut(key), value) {
            (Some(Value::String(have)), Value::String(add))
                if matches!(key.as_str(), "text" | "summary" | "data") =>
            {
                have.push_str(add);
            }
            (Some(have), value) if have.is_null() => *have = value.clone(),
            (None, value) => {
                open.insert(key.clone(), value.clone());
            }
            _ => {}
        }
    }
}

fn stop(reason: &str) -> Stop {
    let kind = match reason {
        "stop" => StopReason::EndTurn,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal,
        _ => StopReason::Other,
    };
    Stop {
        reason: kind,
        details: (kind == StopReason::Other).then(|| reason.to_owned()),
    }
}

/// `prompt_tokens` включает прочитанное и записанное в кэш — ядро складывает их обратно в якоре.
fn usage_of(usage: &Value, cache: CacheMode) -> Usage {
    let number = |path: &[&str]| {
        path.iter()
            .try_fold(usage, |v, key| v.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let prompt = number(&["prompt_tokens"]);
    let cached = number(&["prompt_tokens_details", "cached_tokens"]);
    let written = number(&["prompt_tokens_details", "cache_write_tokens"]);
    // Все breakpoints запроса несут один TTL, так что запись целиком его.
    let (cache_write_short, cache_write_long) = match cache {
        CacheMode::Long => (0, written),
        CacheMode::Short | CacheMode::Off => (written, 0),
    };
    Usage {
        input: prompt.saturating_sub(cached).saturating_sub(written),
        cache_read: cached,
        cache_write_short,
        cache_write_long,
        output: number(&["completion_tokens"]),
        reasoning: number(&["completion_tokens_details", "reasoning_tokens"]),
        cost: usage.get("cost").and_then(Value::as_f64),
        ..Usage::default()
    }
}

#[derive(Debug, Default)]
struct PendingTool {
    index: u64,
    id: String,
    name: String,
    arguments: String,
}

impl PendingTool {
    /// Кривой JSON — строкой: тул вернёт модели ошибку, тур не падает; пустой id — свой, иначе результат не спарить.
    fn into_item(self) -> GenItem {
        let input = if self.arguments.trim().is_empty() {
            Value::Object(Map::new())
        } else {
            serde_json::from_str(&self.arguments).unwrap_or(Value::String(self.arguments))
        };
        GenItem::ToolUse {
            id: if self.id.is_empty() {
                format!("call_{}", self.index)
            } else {
                self.id
            },
            name: self.name,
            input,
        }
    }
}

/// Инлайн `<think>…</think>` в `content` → thinking; возможное начало тега на
/// конце куска придерживается до следующего.
#[derive(Debug, Default)]
struct ThinkSplit {
    inside: bool,
    held: String,
}

impl ThinkSplit {
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";

    fn push(&mut self, chunk: &str, out: &mut Vec<GenItem>) {
        self.held.push_str(chunk);
        loop {
            let tag = if self.inside { Self::CLOSE } else { Self::OPEN };
            match self.held.find(tag) {
                Some(at) => {
                    self.emit(at, out);
                    self.held.drain(..tag.len());
                    self.inside = !self.inside;
                }
                None => {
                    let keep = (1..tag.len())
                        .rev()
                        .find(|&n| self.held.ends_with(&tag[..n]))
                        .unwrap_or(0);
                    self.emit(self.held.len() - keep, out);
                    return;
                }
            }
        }
    }

    fn finish(&mut self, out: &mut Vec<GenItem>) {
        self.emit(self.held.len(), out);
    }

    fn emit(&mut self, upto: usize, out: &mut Vec<GenItem>) {
        let text: String = self.held.drain(..upto).collect();
        if text.is_empty() {
            return;
        }
        out.push(if self.inside {
            GenItem::ThinkingDelta { text }
        } else {
            GenItem::TextDelta { text }
        });
    }
}
