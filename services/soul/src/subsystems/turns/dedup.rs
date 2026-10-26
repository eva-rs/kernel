//! Ключ дедупа результатов (`01 §4`): `blake3(tool ‖ canonical_json(input) ‖ content)`.

use serde_json::Value;

use eva_protocol::types::ToolId;

/// `canonical_json`: ключи по порядку, без пробелов, числа как в JSON. Порядок
/// ключей пишется явно: `serde_json` с `preserve_order` хранит порядок вставки.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write(value, &mut out);
    out
}

fn write(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// Ключ записи `tool_results`; `extra_tools` тура (`tool_id = None`) различаются каноничным именем.
pub fn dedup_key(
    tool_id: Option<ToolId>,
    canonical: &str,
    input: &Value,
    content: &[u8],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    match tool_id {
        Some(id) => hasher.update(b"tool:").update(&id.0.to_le_bytes()),
        None => hasher.update(b"extra:").update(canonical.as_bytes()),
    };
    hasher
        .update(&[0])
        .update(canonical_json(input).as_bytes())
        .update(&[0])
        .update(content);
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn canonical_json_sorts_keys_and_drops_whitespace() {
        let a = json!({"b": [1, {"z": null, "a": 2.5}], "a": "x y"});
        assert_eq!(
            canonical_json(&a),
            r#"{"a":"x y","b":[1,{"a":2.5,"z":null}]}"#
        );
        let reordered: Value =
            serde_json::from_str(r#"{ "a" : "x y", "b" : [ 1 , { "a" : 2.5, "z" : null } ] }"#)
                .expect("json");
        assert_eq!(canonical_json(&reordered), canonical_json(&a));
    }

    #[test]
    fn key_separates_tool_input_and_content() {
        let input = json!({"path": "/a"});
        let base = dedup_key(Some(ToolId(1)), "fs_read", &input, b"body");
        assert_eq!(
            base,
            dedup_key(Some(ToolId(1)), "renamed", &input, b"body"),
            "tool_id identifies the tool"
        );
        assert_ne!(base, dedup_key(Some(ToolId(2)), "fs_read", &input, b"body"));
        assert_ne!(
            base,
            dedup_key(Some(ToolId(1)), "fs_read", &json!({"path": "/b"}), b"body")
        );
        assert_ne!(
            base,
            dedup_key(Some(ToolId(1)), "fs_read", &input, b"other")
        );
        assert_ne!(base, dedup_key(None, "fs_read", &input, b"body"));
        assert_ne!(
            dedup_key(None, "a", &json!("b"), b""),
            dedup_key(None, "a\"b", &json!(""), b""),
            "fields do not run into each other"
        );
    }
}
