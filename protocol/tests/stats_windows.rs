//! Точные границы расхода сохраняют наносекунды в JSON и бинарном wire.

use eva_protocol::types::stats::SpendWindow;
use serde_json::{Value, json};

fn nanos(window: &SpendWindow) -> (u64, u64) {
    let SpendWindow::Instants { from, to } = window else {
        panic!("instant window")
    };
    (from.as_nanos().get(), to.as_nanos().get())
}

#[test]
fn instant_windows_preserve_fraction_digits_in_json_and_msgpack() {
    for fraction in [1_u32, 10, 10_203_040, 100_000_000, 123_456_789, 999_999_999] {
        let value = json!({"instants": {
            "from": format!("01.01.1970 00:00:00.{fraction:09}"),
            "to": "01.01.1970 00:00:01.123456789"
        }});
        let window: SpendWindow = serde_json::from_value(value.clone()).unwrap();
        let expected = (u64::from(fraction), 1_123_456_789);
        assert_eq!(nanos(&window), expected);
        assert_eq!(serde_json::to_value(&window).unwrap(), value);
        let from_text: SpendWindow = serde_json::from_str(&value.to_string()).unwrap();
        assert_eq!(nanos(&from_text), expected);

        let binary = rmp_serde::to_vec_named(&window).unwrap();
        let decoded: SpendWindow = rmp_serde::from_slice(&binary).unwrap();
        assert_eq!(nanos(&decoded), expected);
        let raw: Value = rmp_serde::from_slice(&binary).unwrap();
        assert_eq!(
            raw,
            json!({"instants": {"from": expected.0, "to": expected.1}})
        );
    }
}

#[test]
fn instant_windows_reject_malformed_or_unrepresentable_strings() {
    for invalid in [
        "01.01.1970 00:00:00.000000000",
        "01.01.2226 00:00:00.000000001",
        "31.02.2026 00:00:00.000000001",
        "01.01.1970 24:00:00.000000001",
        "01.01.1970 00:00:60.000000001",
        "01.01.1970 00:00:00.00000001",
        "01.01.1970 00:00:00.0000000001",
        "01.01.1970 00:00:00.00000000a",
        "01.01.1970 00:00:00.00000000１",
        "01.01.1970 00:00:00.0000000é",
        "01.01.1970T00:00:00.000000001",
        "1970-01-01 00:00:00.000000001",
    ] {
        for field in ["from", "to"] {
            let mut value = json!({"instants": {
                "from": "01.01.1970 00:00:00.000000001",
                "to": "01.01.1970 00:00:01.000000001"
            }});
            value["instants"][field] = json!(invalid);
            assert!(
                serde_json::from_value::<SpendWindow>(value).is_err(),
                "{field}: {invalid}"
            );
        }
    }
}
