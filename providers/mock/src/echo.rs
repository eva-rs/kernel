//! Ответ без сценария и детерминированные эмбеддинги.

use serde_json::{Map, Value, json};

use eva_protocol::types::generate::{
    Block, GenerateRequest, Message, ResultBlock, StopReason, Usage,
};

use crate::{Script, tokens};

/// Образец по `format`, иначе эхо последней user-реплики; текст режется по словам — стрим виден клиенту.
pub(crate) fn script(req: &GenerateRequest, input: u64) -> Script {
    let reply = match &req.format {
        Some(format) => sample_from_schema(&format.schema).to_string(),
        None => format!("[mock:{}] echo: {}", req.model, last_user_text(req)),
    };
    let output = tokens(reply.len());
    reply
        .split_inclusive(' ')
        .fold(Script::new(), Script::text)
        .usage(Usage {
            input,
            output,
            ..Usage::default()
        })
        .stop(StopReason::EndTurn)
}

fn last_user_text(req: &GenerateRequest) -> String {
    let Some(blocks) = req.messages.iter().rev().find_map(|m| match m {
        Message::User { blocks } => Some(blocks),
        _ => None,
    }) else {
        return String::new();
    };
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            Block::Text { text } => parts.push(text.as_str()),
            Block::ToolResult { content, .. } => {
                parts.extend(content.iter().filter_map(|c| match c {
                    ResultBlock::Text { text } => Some(text.as_str()),
                    ResultBlock::Image { .. } => None,
                }));
            }
            _ => {}
        }
    }
    parts.join(" ")
}

/// Минимальный валидный образец: `const`/`enum` — первое значение, объект —
/// только обязательные поля, массив пуст, скаляры — нули.
pub fn sample_from_schema(schema: &Value) -> Value {
    if let Some(constant) = schema.get("const") {
        return constant.clone();
    }
    if let Some(first) = schema
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
    {
        return first.clone();
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(first) = schema
            .get(key)
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        {
            return sample_from_schema(first);
        }
    }
    let kind = match schema.get("type") {
        Some(Value::String(kind)) => Some(kind.as_str()),
        Some(Value::Array(kinds)) => kinds.first().and_then(Value::as_str),
        _ => None,
    };
    match kind {
        Some("array") => json!([]),
        Some("string") => json!(""),
        Some("integer" | "number") => schema.get("minimum").cloned().unwrap_or(json!(0)),
        Some("boolean") => json!(false),
        Some("null") => Value::Null,
        _ => {
            let properties = schema.get("properties").and_then(Value::as_object);
            let required = schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str);
            let mut out = Map::new();
            for name in required {
                let sub = properties.and_then(|p| p.get(name));
                out.insert(name.to_owned(), sub.map_or(Value::Null, sample_from_schema));
            }
            Value::Object(out)
        }
    }
}

/// Единичный вектор из хэша текста: одинаковый вход — одинаковый вектор на любой сборке.
pub(crate) fn vector(text: &str, dims: u32) -> Vec<f32> {
    let mut state = fnv1a(text.as_bytes());
    let mut vector: Vec<f32> = (0..dims)
        .map(|_| {
            let bits = splitmix64(&mut state) >> 40;
            (bits as f32 / (1_u64 << 24) as f32).mul_add(2.0, -1.0)
        })
        .collect();
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        vector.iter_mut().for_each(|x| *x /= norm);
    }
    vector
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}
