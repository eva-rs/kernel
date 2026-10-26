//! Типизированный контекст тула, преобразование результата и схема входа.

use std::{fmt, pin::Pin, sync::Arc};

use eva::_priv::schemars::{JsonSchema, generate::SchemaSettings};
use serde_json::Value;

use eva_protocol::{
    types::turn::TurnCtx,
    wire::{ErrorCode, RpcError, tools::ToolResult},
};
use eva_rpc::Progress;

use crate::Soul;

pub(crate) type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Вызов тула: сессия soul, тур и прогресс.
pub struct ToolCtx<S = ()> {
    call_id: String,
    turn: TurnCtx,
    soul: Soul,
    progress: Progress,
    state: Arc<S>,
    translations: Arc<crate::Catalogs>,
    domain: Arc<str>,
}

impl ToolCtx {
    pub(crate) fn new(call_id: String, turn: TurnCtx, soul: Soul, progress: Progress) -> Self {
        Self {
            call_id,
            turn,
            soul,
            progress,
            state: Arc::new(()),
            translations: Arc::new(crate::Catalogs::default()),
            domain: Arc::from(""),
        }
    }
}

impl<S> ToolCtx<S> {
    pub fn state(&self) -> &S {
        &self.state
    }

    pub(crate) fn with_state<T>(self, state: Arc<T>) -> ToolCtx<T> {
        ToolCtx {
            call_id: self.call_id,
            turn: self.turn,
            soul: self.soul,
            progress: self.progress,
            state,
            translations: self.translations,
            domain: self.domain,
        }
    }

    pub(crate) fn with_translations(
        mut self,
        translations: Arc<crate::Catalogs>,
        domain: Arc<str>,
    ) -> Self {
        self.translations = translations;
        self.domain = domain;
        self
    }

    /// Soul через текущее соединение: тул переживает реконнект, ручка тоже.
    pub fn soul(&self) -> Soul {
        self.soul.clone()
    }

    pub fn turn(&self) -> &TurnCtx {
        &self.turn
    }

    /// `tool_use.id` модели: ключ дедупа повтора после реконнекта.
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// Статус в ленту тура; Message сохраняет переводы для выбора языка получателем.
    pub async fn progress(&self, text: impl Into<ProgressText>) {
        match text.into() {
            ProgressText::Plain(text) => self.progress.send(text).await,
            ProgressText::Message(message) => {
                self.progress
                    .send_localized(self.translations.bind(&self.domain, message))
                    .await
            }
        }
    }
}

impl<S> fmt::Debug for ToolCtx<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolCtx")
            .field("call_id", &self.call_id)
            .field("turn", &self.turn.turn_id)
            .finish_non_exhaustive()
    }
}

/// Простой текст либо сообщение с отложенным выбором языка.
pub enum ProgressText {
    Plain(String),
    Message(crate::Message),
}
impl From<String> for ProgressText {
    fn from(value: String) -> Self {
        Self::Plain(value)
    }
}
impl From<&str> for ProgressText {
    fn from(value: &str) -> Self {
        Self::Plain(value.into())
    }
}
impl From<&String> for ProgressText {
    fn from(value: &String) -> Self {
        Self::Plain(value.clone())
    }
}
impl From<crate::Message> for ProgressText {
    fn from(value: crate::Message) -> Self {
        Self::Message(value)
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
    pub run: ToolFn,
}

/// Путь `group.sub.tool` → каноничное имя `group_sub_tool` (`02 §1`).
pub use eva_protocol::types::tool::canonical;

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

/// Модель пишет числа и булевы в кавычках — строка
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
