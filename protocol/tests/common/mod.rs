//! Полнота выборок: теги вариантов сверяются со схемой, новый вариант без
//! образца валит тест.

use std::collections::BTreeSet;

use eva::_priv::schemars::{JsonSchema, schema_for};
use serde::Serialize;
use serde_json::Value;

/// Теги внешне-тегированного enum'а по его схеме: ключ data-варианта, имя unit-варианта.
pub fn schema_variants<T: JsonSchema>() -> BTreeSet<String> {
    let schema = serde_json::to_value(schema_for!(T)).expect("schema");
    let arms = schema
        .get("oneOf")
        .or_else(|| schema.get("anyOf"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| vec![schema.clone()]);

    let mut tags = BTreeSet::new();
    for arm in &arms {
        let names = arm["required"]
            .get(0)
            .into_iter()
            .chain([&arm["const"]])
            .chain(arm["enum"].as_array().into_iter().flatten());
        tags.extend(names.filter_map(Value::as_str).map(str::to_owned));
    }
    tags
}

/// Тег значения на проводе: имя unit-варианта либо единственный ключ карты.
pub fn wire_tag<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value).expect("json") {
        Value::String(tag) => tag,
        Value::Object(map) if map.len() == 1 => map.into_iter().map(|(tag, _)| tag).collect(),
        other => panic!("not an externally tagged enum: {other}"),
    }
}

pub fn assert_every_variant<'a, T: JsonSchema + Serialize + 'a>(
    samples: impl IntoIterator<Item = &'a T>,
) {
    let covered: BTreeSet<String> = samples.into_iter().map(wire_tag).collect();
    assert_eq!(
        covered,
        schema_variants::<T>(),
        "{} needs a sample of every variant",
        std::any::type_name::<T>()
    );
}
