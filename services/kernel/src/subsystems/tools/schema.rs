//! Схема входа тула ядра из `#[data]`-типа (`02 §1.1`) и проверка входа
//! модели по схеме с приведением (`02 §4.2` п.3): дважды закодированный
//! JSON разворачивается, строки приводятся к number/boolean/integer,
//! `Option` принимает `null`, лишние поля отбрасываются, отсутствие
//! `required` называет поле и ближайшее присланное.

use eva::_priv::schemars::{JsonSchema, generate::SchemaSettings};
use serde_json::{Map, Value};

use crate::subsystems::tools::step::nearest;

/// Draft 2020-12, подсхемы inline, без `title` и `$schema`.
pub fn input_schema<I: JsonSchema>() -> Value {
    let schema = SchemaSettings::draft2020_12()
        .with(|s| s.inline_subschemas = true)
        .into_generator()
        .into_root_schema_for::<I>();
    let mut value = schema.to_value();
    strip(&mut value);
    if let Value::Object(object) = &mut value {
        object.remove("$schema");
        object
            .entry("properties")
            .or_insert_with(|| Value::Object(Map::new()));
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

/// Вход, приведённый к схеме; `Err` — текст `is_error` модели.
pub fn check(input: Value, schema: &Value) -> Result<Value, String> {
    let mut input = match input {
        Value::String(text) => match serde_json::from_str::<Value>(&text) {
            Ok(object @ Value::Object(_)) => object,
            _ => return Err(String::from("invalid input: expected a JSON object")),
        },
        Value::Null => Value::Object(Map::new()),
        other => other,
    };
    let Value::Object(object) = &mut input else {
        return Err(String::from("invalid input: expected a JSON object"));
    };
    let Some(Value::Object(spec)) = Some(schema) else {
        return Ok(input);
    };
    shape(object, spec)?;
    Ok(input)
}

fn shape(object: &mut Map<String, Value>, schema: &Map<String, Value>) -> Result<(), String> {
    let properties = schema.get("properties").and_then(Value::as_object);
    let mut dropped: Vec<String> = Vec::new();
    if let Some(properties) = properties {
        let open = schema
            .get("additionalProperties")
            .is_some_and(|a| a != &Value::Bool(false));
        if !open {
            dropped = object
                .keys()
                .filter(|k| !properties.contains_key(*k))
                .cloned()
                .collect();
            object.retain(|key, _| properties.contains_key(key));
        }
        for (name, field) in object.iter_mut() {
            if let Some(field_schema) = properties.get(name) {
                coerce(field, field_schema);
            }
        }
    }
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    for name in required {
        let missing = match object.get(name) {
            None => true,
            Some(Value::Null) => !nullable(properties.and_then(|p| p.get(name))),
            Some(_) => false,
        };
        if missing {
            let hint = nearest(dropped.iter().map(String::as_str), name)
                .into_iter()
                .next()
                .map(|sent| format!(" (got unknown field \"{sent}\")"))
                .unwrap_or_default();
            return Err(format!(
                "invalid input: missing required field \"{name}\"{hint}"
            ));
        }
    }
    Ok(())
}

fn nullable(schema: Option<&Value>) -> bool {
    let Some(Value::Object(schema)) = schema else {
        return false;
    };
    let is_null = |t: &Value| t == "null";
    match schema.get("type") {
        Some(Value::Array(types)) if types.iter().any(is_null) => return true,
        Some(t) if is_null(t) => return true,
        _ => {}
    }
    ["anyOf", "oneOf"]
        .into_iter()
        .filter_map(|k| schema.get(k).and_then(Value::as_array))
        .flatten()
        .any(|b| nullable(Some(b)))
}

/// Строка — к типу схемы, где разбирается однозначно; вглубь объектов и массивов.
pub fn coerce(value: &mut Value, schema: &Value) {
    let Value::Object(schema) = schema else {
        return;
    };
    let branches = ["anyOf", "oneOf"]
        .into_iter()
        .filter_map(|key| schema.get(key).and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_object);

    match value {
        Value::String(text) => {
            let coerced =
                scalar(text, schema).or_else(|| branches.clone().find_map(|b| scalar(text, b)));
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

fn scalar(text: &str, schema: &Map<String, Value>) -> Option<Value> {
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
    if types.contains(&"object") || types.contains(&"array") {
        return serde_json::from_str(text).ok();
    }
    None
}

#[cfg(test)]
mod tests {
    use eva::data;
    use serde_json::json;

    use super::*;

    /// Read lines of a file.
    #[data]
    struct Read {
        /// Path to read.
        path: String,
        offset: Option<u32>,
        verbose: bool,
    }

    #[test]
    fn coercion_and_required_hint() {
        let schema = input_schema::<Read>();
        assert!(schema.get("title").is_none() && schema.get("$schema").is_none());
        assert!(schema.get("properties").is_some());

        let ok = check(
            json!({"path": "/a", "offset": "7", "verbose": "true", "junk": 1}),
            &schema,
        )
        .expect("coerced");
        assert_eq!(ok, json!({"path": "/a", "offset": 7, "verbose": true}));

        let double =
            check(json!(r#"{"path": "/b", "verbose": false}"#), &schema).expect("unwrapped");
        assert_eq!(double["path"], "/b");

        let null = check(
            json!({"path": "/c", "offset": null, "verbose": false}),
            &schema,
        )
        .expect("null for Option");
        assert_eq!(null["offset"], Value::Null);

        let err = check(json!({"pth": "/a", "verbose": true}), &schema).expect_err("missing");
        assert!(
            err.contains("missing required field \"path\" (got unknown field \"pth\")"),
            "{err}"
        );

        assert!(check(json!([1]), &schema).is_err());
    }
}
