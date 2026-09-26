//! Тулы модуля (`02 §1.1`, `06 §4`): декларация из `#[data]`-типа входа и
//! обработчик. Ядро зовёт тулы модулей с M4; до того они только объявлены.

use std::{fmt, pin::Pin, sync::Arc};

use eva::_priv::schemars::{JsonSchema, generate::SchemaSettings};
use serde::de::DeserializeOwned;
use serde_json::Value;

use eva_protocol::{
    types::{tool::ToolDecl, turn::TurnCtx},
    wire::{ErrorCode, RpcError, tools::ToolResult},
};
use eva_rpc::{Progress, RpcClient};

use crate::Kernel;

pub(crate) type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Вызов тула: сессия ядра, тур и прогресс.
pub struct ToolCtx {
    call_id: String,
    turn: TurnCtx,
    client: RpcClient,
    progress: Progress,
}

impl ToolCtx {
    pub(crate) fn new(
        call_id: String,
        turn: TurnCtx,
        client: RpcClient,
        progress: Progress,
    ) -> Self {
        Self {
            call_id,
            turn,
            client,
            progress,
        }
    }

    /// Сессия соединения, по которому пришёл вызов.
    pub fn kernel(&self) -> Kernel {
        self.client.session()
    }

    pub fn turn(&self) -> &TurnCtx {
        &self.turn
    }

    /// `tool_use.id` модели: ключ дедупа повтора после реконнекта.
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// `progress{text}` в ленту items; ушедший потребитель теряет текст молча.
    pub async fn progress(&self, text: impl Into<String>) {
        self.progress.send(text).await;
    }
}

impl fmt::Debug for ToolCtx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolCtx")
            .field("call_id", &self.call_id)
            .field("turn", &self.turn.turn_id)
            .finish_non_exhaustive()
    }
}

/// Ошибка обработчика тула. Любая `Display`-ошибка через `?` — `Respond`:
/// текст уходит модели как `is_error`, тур продолжается. `fatal` рвёт тур
/// (`stream_end{error}`, `02 §4.2`).
///
/// `Display` у типа нет намеренно: иначе blanket-`From` конфликтовал бы с `From<T> for T`.
#[derive(Debug)]
pub struct ToolError(Kind);

#[derive(Debug)]
enum Kind {
    Respond(String),
    Fatal(RpcError),
}

impl ToolError {
    /// Данные для модели: тур продолжается.
    pub fn respond(text: impl Into<String>) -> Self {
        Self(Kind::Respond(text.into()))
    }

    /// Тур завершается ошибкой.
    pub fn fatal(reason: impl fmt::Display) -> Self {
        Self(Kind::Fatal(RpcError::new(ErrorCode::Internal, reason)))
    }

    pub(crate) fn into_outcome(self) -> Result<ToolResult, RpcError> {
        match self.0 {
            Kind::Respond(text) => Ok(ToolResult::error(text)),
            Kind::Fatal(error) => Err(error),
        }
    }
}

impl<E: fmt::Display> From<E> for ToolError {
    fn from(error: E) -> Self {
        Self::respond(error.to_string())
    }
}

/// Стёртый обработчик: вход — JSON аргументов модели.
pub(crate) type ToolFn =
    Arc<dyn Fn(ToolCtx, Value) -> BoxFuture<Result<ToolResult, RpcError>> + Send + Sync>;

pub(crate) struct ToolEntry {
    pub decl: ToolDecl,
    pub run: ToolFn,
}

/// Путь `group.sub.tool` → каноничное имя `group_sub_tool` (`02 §1`).
pub fn canonical(path: &str) -> String {
    path.replace('.', "_")
}

/// Схема входа: draft 2020-12, вложенные схемы inline, без `title`.
pub fn input_schema<I: JsonSchema>() -> Value {
    let schema = SchemaSettings::draft2020_12()
        .with(|s| s.inline_subschemas = true)
        .into_generator()
        .into_root_schema_for::<I>();
    let mut value = schema.to_value();
    strip(&mut value);
    if let Value::Object(object) = &mut value {
        // Диалект фиксирует протокол; в спеке тула на каждом шаге это лишние токены.
        object.remove("$schema");
    }
    value
}

/// Ключевые слова схемы, чьи значения — данные, а не подсхемы.
const DATA_KEYWORDS: &[&str] = &["default", "examples", "const", "enum"];

fn strip(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.remove("title");
            for (key, child) in object.iter_mut() {
                if DATA_KEYWORDS.contains(&key.as_str()) {
                    continue;
                }
                if key == "properties" || key == "patternProperties" || key == "$defs" {
                    if let Value::Object(schemas) = child {
                        schemas.values_mut().for_each(strip);
                    }
                    continue;
                }
                strip(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip),
        _ => {}
    }
}

/// Модель пишет числа и булевы в кавычках (legacy: id в кавычках) — строка
/// приводится к типу схемы, где разбирается однозначно.
pub fn coerce(value: &mut Value, schema: &Value) {
    let Value::Object(schema) = schema else {
        return;
    };

    // Ветки `anyOf`/`oneOf` — `Option<T>` и untagged-варианты: первая подходящая.
    let branches = ["anyOf", "oneOf"]
        .into_iter()
        .filter_map(|key| schema.get(key).and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_object);

    match value {
        Value::String(text) => {
            let coerced = coerce_scalar(text, schema)
                .or_else(|| branches.clone().find_map(|b| coerce_scalar(text, b)));
            if let Some(coerced) = coerced {
                *value = coerced;
            }
        }
        Value::Object(object) => {
            let properties = std::iter::once(schema)
                .chain(branches)
                .find_map(|s| s.get("properties").and_then(Value::as_object));
            if let Some(properties) = properties {
                for (name, field) in object.iter_mut() {
                    if let Some(field_schema) = properties.get(name) {
                        coerce(field, field_schema);
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                items.iter_mut().for_each(|item| coerce(item, item_schema));
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn coerce_scalar(text: &str, schema: &serde_json::Map<String, Value>) -> Option<Value> {
    let types: Vec<&str> = match schema.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => return None,
    };
    if types.contains(&"string") {
        return None;
    }
    let text = text.trim();
    if types.contains(&"integer")
        && let Ok(n) = text.parse::<i64>()
    {
        return Some(Value::from(n));
    }
    if types.contains(&"number")
        && let Ok(n) = text.parse::<f64>()
        && let Some(n) = serde_json::Number::from_f64(n)
    {
        return Some(Value::Number(n));
    }
    if types.contains(&"boolean") {
        match text {
            "true" => return Some(Value::Bool(true)),
            "false" => return Some(Value::Bool(false)),
            _ => {}
        }
    }
    None
}

/// Обработчик пользователя → стёртый: приведение и разбор входа, отказ
/// разбора — `is_error` модели, чтобы она исправила аргументы.
pub(crate) fn erase<I, E, F, Fut>(schema: Value, handler: F) -> ToolFn
where
    I: DeserializeOwned + Send + 'static,
    E: Into<ToolError> + 'static,
    F: Fn(ToolCtx, I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ToolResult, E>> + Send + 'static,
{
    let handler = Arc::new(handler);
    Arc::new(move |ctx: ToolCtx, mut input: Value| {
        coerce(&mut input, &schema);
        let parsed = serde_json::from_value::<I>(input);
        let handler = handler.clone();
        Box::pin(async move {
            let input = match parsed {
                Ok(input) => input,
                Err(e) => return Ok(ToolResult::error(format!("invalid input: {e}"))),
            };
            match handler(ctx, input).await {
                Ok(result) => Ok(result),
                Err(error) => error.into().into_outcome(),
            }
        })
    })
}
