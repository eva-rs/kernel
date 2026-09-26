//! `GenerateRequest` → тело `POST /chat/completions`: сообщения рендерятся
//! независимо от соседей, поэтому префикс лога даёт префикс `messages` (I1),
//! а летучесть одна — `cache_control` хвостового breakpoint'а.

use std::collections::BTreeSet;

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Map, Value, json};

use eva_protocol::types::{
    Bytes, Effort, ModelId, Thinking,
    generate::{
        Block, CacheMode, GenerateRequest, Message, Position, Purpose, ResultBlock, ToolSpec,
    },
    model::ModelDecl,
};

use crate::{
    config::Dialect,
    error::{invalid, other},
};

pub fn render(
    req: &GenerateRequest,
    model: &ModelDecl,
    dialect: Dialect,
) -> Result<Value, crate::ProviderError> {
    if req.purpose == Purpose::KeepAlive {
        return Err(invalid(
            "keep_alive request: chat completions has no cache_keep_alive",
        ));
    }
    let router = dialect == Dialect::Openrouter;

    let mut body = Map::new();
    body.insert("model".into(), req.model.as_str().into());
    body.insert(
        "messages".into(),
        Value::Array(messages(req, model, dialect)?),
    );
    body.insert("stream".into(), true.into());
    body.insert("stream_options".into(), json!({"include_usage": true}));
    if router {
        body.insert("usage".into(), json!({"include": true}));
    }
    if req.max_tokens > 0 {
        body.insert("max_tokens".into(), req.max_tokens.into());
    }

    let tools = tools(&req.tools);
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }

    if model.caps.thinking {
        let level = |effort| effort_level(effort, model);
        match (router, req.thinking, req.effort) {
            (true, Some(Thinking::Off), _) => {
                body.insert("reasoning".into(), json!({"enabled": false}));
            }
            (true, _, Some(effort)) => {
                body.insert("reasoning".into(), json!({"effort": level(effort)}));
            }
            (true, Some(Thinking::Adaptive), None) => {
                body.insert("reasoning".into(), json!({"enabled": true}));
            }
            (false, thinking, Some(effort)) if thinking != Some(Thinking::Off) => {
                body.insert("reasoning_effort".into(), level(effort).into());
            }
            _ => {}
        }
    }

    if let Some(format) = &req.format {
        if model.caps.structured_output {
            body.insert(
                "response_format".into(),
                json!({
                    "type": "json_schema",
                    "json_schema": {
                        "name": "output",
                        "strict": format.strict,
                        "schema": normalize_schema(&format.schema),
                    },
                }),
            );
        } else {
            log::debug!(model = req.model.as_str(); "output format dropped: model has no structured_output");
        }
    }

    if model.caps.image_out {
        body.insert("modalities".into(), json!(["image", "text"]));
    }

    Ok(Value::Object(body))
}

fn effort_level(effort: Effort, model: &ModelDecl) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::Max if model.caps.effort_levels.contains(&Effort::Max) => "xhigh",
        Effort::Max => "high",
    }
}

/// Отложенные тулы не шлются: `defer_loading` в Chat Completions нет.
fn tools(specs: &[ToolSpec]) -> Vec<Value> {
    specs
        .iter()
        .filter(|t| !t.deferred)
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": normalize_schema(&t.input_schema),
                },
            })
        })
        .collect()
}

/// `oneOf` → `anyOf`: часть строгих валидаторов (Moonshot) отвергает `oneOf`,
/// а ветки наших юнионов взаимоисключающи по форме — смысл тот же.
pub fn normalize_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, value) in map {
                let value = normalize_schema(value);
                if key == "oneOf" || key == "anyOf" {
                    match (out.get_mut("anyOf"), value) {
                        (Some(Value::Array(existing)), Value::Array(more)) => existing.extend(more),
                        (_, value) => {
                            out.insert("anyOf".into(), value);
                        }
                    }
                } else {
                    out.insert(key.clone(), value);
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(normalize_schema).collect()),
        other => other.clone(),
    }
}

fn messages(
    req: &GenerateRequest,
    model: &ModelDecl,
    dialect: Dialect,
) -> Result<Vec<Value>, crate::ProviderError> {
    let marker = cache_marker(req, model, dialect)?;
    let marked = |position: Position| {
        marker
            .as_ref()
            .filter(|_| req.breakpoints.contains(&position))
    };

    let mut out = Vec::with_capacity(req.messages.len() + 1);
    if let Some(system) = system(req, dialect, marked(Position::System)) {
        out.push(system);
    }
    let mut wire = Vec::new();
    for (index, message) in req.messages.iter().enumerate() {
        render_message(message, req, model, dialect, &mut wire)?;
        let position = Position::Message {
            index: u32::try_from(index).map_err(|_| invalid("too many messages"))?,
        };
        // Сообщение без текстовой части (assistant из одних tool_calls) breakpoint честно не несёт.
        if let (Some(marker), Some(last)) = (marked(position), wire.last_mut()) {
            last.mark(marker);
        }
        out.extend(wire.drain(..).map(Wire::into_value));
    }
    Ok(out)
}

fn cache_marker(
    req: &GenerateRequest,
    model: &ModelDecl,
    dialect: Dialect,
) -> Result<Option<Value>, crate::ProviderError> {
    let limit = model.caps.cache_breakpoints;
    if dialect == Dialect::Plain || limit == 0 {
        return Ok(None);
    }
    let marker = match req.cache {
        CacheMode::Off => return Ok(None),
        CacheMode::Short => json!({"type": "ephemeral"}),
        CacheMode::Long => json!({"type": "ephemeral", "ttl": "1h"}),
    };
    if req.breakpoints.len() > usize::from(limit) {
        return Err(invalid(format!(
            "{} breakpoints, model {} takes {limit}",
            req.breakpoints.len(),
            req.model
        )));
    }
    let distinct: BTreeSet<_> = req.breakpoints.iter().collect();
    if distinct.len() != req.breakpoints.len() {
        return Err(invalid("duplicated breakpoint"));
    }
    for position in &req.breakpoints {
        if let Position::Message { index } = *position
            && usize::try_from(index).map_or(true, |i| i >= req.messages.len())
        {
            return Err(invalid(format!(
                "breakpoint at message {index} is out of range"
            )));
        }
    }
    Ok(Some(marker))
}

/// `plain` — одна строка: шаблоны локальных серверов не всегда переваривают массив частей в system.
fn system(req: &GenerateRequest, dialect: Dialect, marker: Option<&Value>) -> Option<Value> {
    let texts: Vec<&str> = req
        .system
        .iter()
        .map(|s| s.text.as_str())
        .filter(|t| !t.is_empty())
        .collect();
    if texts.is_empty() {
        return None;
    }
    let mut wire = Wire::new("system");
    match dialect {
        Dialect::Plain => wire.text(&texts.join("\n\n")),
        Dialect::Openrouter => texts.iter().for_each(|t| wire.text(t)),
    }
    if let Some(marker) = marker {
        wire.mark(marker);
    }
    Some(wire.into_value())
}

fn render_message(
    message: &Message,
    req: &GenerateRequest,
    model: &ModelDecl,
    dialect: Dialect,
    wire: &mut Vec<Wire>,
) -> Result<(), crate::ProviderError> {
    match message {
        Message::User { blocks } => user(blocks, wire),
        Message::Assistant { blocks } => assistant(blocks, req, model, dialect, wire),
        Message::System {
            blocks,
            clear_at,
            output_config,
            tool_changes,
            ..
        } => {
            if !model.caps.system_in_messages {
                return Err(invalid(format!(
                    "system message for {}, which has no system_in_messages",
                    req.model
                )));
            }
            if clear_at.is_some() || output_config.is_some() || tool_changes.is_some() {
                return Err(invalid(
                    "chat completions has no clear_at, output_config or tool_changes",
                ));
            }
            let mut system = Wire::new("system");
            for block in blocks {
                match block {
                    Block::Text { text } => system.text(text),
                    _ => return Err(invalid("system message carries a non-text block")),
                }
            }
            if !system.parts.is_empty() {
                wire.push(system);
            }
            Ok(())
        }
    }
}

/// `tool_result` — сообщения `role: tool` сразу после вызова (картинок там нет
/// в формате); текст и все картинки, включая картинки результатов, — следом одним user-сообщением.
fn user(blocks: &[Block], wire: &mut Vec<Wire>) -> Result<(), crate::ProviderError> {
    let mut rest = Wire::new("user");
    for block in blocks {
        match block {
            Block::Text { text } => rest.text(text),
            Block::Image { media_type, data } => rest.image(media_type, data),
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let mut tool = Wire::new("tool");
                tool.fields
                    .insert("tool_call_id".into(), tool_use_id.as_str().into());
                let mut prefixed = !is_error;
                for part in content {
                    match part {
                        ResultBlock::Text { text } if !prefixed => {
                            tool.text(&format!("error: {text}"));
                            prefixed = true;
                        }
                        ResultBlock::Text { text } => tool.text(text),
                        ResultBlock::Image { media_type, data } => rest.image(media_type, data),
                    }
                }
                if !prefixed {
                    tool.text("error");
                }
                wire.push(tool);
            }
            Block::ToolUse { .. } | Block::Thinking { .. } | Block::RedactedThinking { .. } => {
                return Err(invalid("tool_use or thinking inside a user message"));
            }
        }
    }
    if !rest.parts.is_empty() {
        wire.push(rest);
    }
    Ok(())
}

fn assistant(
    blocks: &[Block],
    req: &GenerateRequest,
    model: &ModelDecl,
    dialect: Dialect,
    wire: &mut Vec<Wire>,
) -> Result<(), crate::ProviderError> {
    let router = dialect == Dialect::Openrouter;
    let mut message = Wire::new("assistant");
    let mut calls = Vec::new();
    let mut details = Vec::new();
    for block in blocks {
        match block {
            Block::Text { text } => message.text(text),
            // Картинку в assistant принимает только модель, которая сама их рисует.
            Block::Image { media_type, data } if router && model.caps.image_out => {
                message.image(media_type, data);
            }
            Block::Image { .. } => {}
            Block::ToolUse { id, name, input } => calls.push(json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments(input)},
            })),
            Block::Thinking {
                signature: blob,
                model: from,
                ..
            }
            | Block::RedactedThinking {
                data: blob,
                model: from,
            } => {
                details.extend(replay(router, &req.model, from, blob));
            }
            Block::ToolResult { .. } => {
                return Err(invalid("tool_result inside an assistant message"));
            }
        }
    }
    // Одни мысли без ответа: отвечать нечему, а пустой content вендоры отвергают.
    if message.parts.is_empty() && calls.is_empty() {
        return Ok(());
    }
    if !calls.is_empty() {
        message
            .fields
            .insert("tool_calls".into(), Value::Array(calls));
    }
    if !details.is_empty() {
        message
            .fields
            .insert("reasoning_details".into(), Value::Array(details));
    }
    wire.push(message);
    Ok(())
}

/// Кривые аргументы стрим сохранил строкой — модели уходят те же байты, что она написала.
fn arguments(input: &Value) -> String {
    match input {
        Value::String(raw) => raw.clone(),
        other => other.to_string(),
    }
}

/// Элемент `reasoning_details` своей модели как есть; чужой модели и не наш блоб — молча мимо.
fn replay(router: bool, model: &ModelId, from: &ModelId, blob: &Bytes) -> Option<Value> {
    if !router || model != from {
        return None;
    }
    serde_json::from_slice::<Value>(blob)
        .ok()
        .filter(Value::is_object)
}

pub(crate) fn data_url(media_type: &str, data: &[u8]) -> String {
    format!("data:{media_type};base64,{}", STANDARD.encode(data))
}

/// `data:<mime>;base64,<payload>` → (mime, байты).
pub(crate) fn parse_data_url(url: &str) -> Result<(String, Bytes), crate::ProviderError> {
    let (media_type, payload) = url
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(";base64,"))
        .filter(|(media_type, _)| !media_type.is_empty())
        .ok_or_else(|| other("image is not a base64 data url"))?;
    let data = STANDARD
        .decode(payload.trim())
        .map_err(|e| other(format!("image data url: {e}")))?;
    Ok((media_type.to_owned(), Bytes(data)))
}

/// Сообщение до сборки: части нужны, чтобы повесить `cache_control` до решения «строка или массив».
struct Wire {
    role: &'static str,
    parts: Vec<Value>,
    fields: Map<String, Value>,
}

impl Wire {
    fn new(role: &'static str) -> Self {
        Self {
            role,
            parts: Vec::new(),
            fields: Map::new(),
        }
    }

    /// Пустой текстовый блок Anthropic отвергает.
    fn text(&mut self, text: &str) {
        if !text.is_empty() {
            self.parts.push(json!({"type": "text", "text": text}));
        }
    }

    fn image(&mut self, media_type: &str, data: &[u8]) {
        self.parts.push(json!({
            "type": "image_url",
            "image_url": {"url": data_url(media_type, data)},
        }));
    }

    /// Только на текстовую часть: `cache_control` на `image_url` шлюзы понимают непредсказуемо.
    fn mark(&mut self, marker: &Value) {
        if let Some(Value::Object(part)) = self.parts.last_mut()
            && part.get("type").and_then(Value::as_str) == Some("text")
        {
            part.insert("cache_control".into(), marker.clone());
        }
    }

    /// Одна голая текстовая часть — строка: так её понимает любой сервер.
    fn into_value(self) -> Value {
        let content = match self.parts.as_slice() {
            [] if self.role == "assistant" => Value::Null,
            [] => Value::String(String::new()),
            [Value::Object(part)]
                if part.len() == 2 && part.get("type").and_then(Value::as_str) == Some("text") =>
            {
                part.get("text").cloned().unwrap_or_default()
            }
            _ => Value::Array(self.parts),
        };
        let mut message = self.fields;
        message.insert("role".into(), self.role.into());
        message.insert("content".into(), content);
        Value::Object(message)
    }
}
