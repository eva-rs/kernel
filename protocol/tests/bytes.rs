//! `Bytes`: msgpack `bin`, не массив чисел; JSON — base64, как в схеме.

use proptest::prelude::*;
use serde_json::json;

use eva_protocol::types::{Bytes, generate::GenItem, tool::ToolContent};

const BIN8: u8 = 0xc4;
const BIN16: u8 = 0xc5;
const BIN32: u8 = 0xc6;

/// Маркер `bin` нужной ширины и сами байты сразу за заголовком.
fn assert_bin(encoded: &[u8], raw: &[u8]) {
    let header = match raw.len() {
        0..=0xff => {
            assert_eq!(encoded[0], BIN8);
            2
        }
        0x100..=0xffff => {
            assert_eq!(encoded[0], BIN16);
            3
        }
        _ => {
            assert_eq!(encoded[0], BIN32);
            5
        }
    };
    assert_eq!(&encoded[header..], raw);
}

proptest! {
    #[test]
    fn msgpack_bin(raw in prop_oneof![
        proptest::collection::vec(any::<u8>(), 0..300),
        proptest::collection::vec(any::<u8>(), 60_000..70_000),
    ]) {
        let bytes = Bytes::from(raw.clone());
        let encoded = rmp_serde::to_vec_named(&bytes).unwrap();
        assert_bin(&encoded, &raw);
        let back: Bytes = rmp_serde::from_slice(&encoded).unwrap();
        prop_assert_eq!(back.as_slice(), raw.as_slice());

        // Внутри tolerant-enum'а байты идут тем же `bin` без буфера.
        let content = ToolContent::Image { media_type: "image/png".into(), data: bytes };
        let encoded = rmp_serde::to_vec_named(&content).unwrap();
        let back: ToolContent = rmp_serde::from_slice(&encoded).unwrap();
        prop_assert_eq!(back, content);
    }

    #[test]
    fn json_base64(raw in proptest::collection::vec(any::<u8>(), 0..200)) {
        let bytes = Bytes::from(raw);
        let json = serde_json::to_value(&bytes).unwrap();
        prop_assert!(json.is_string());
        let back: Bytes = serde_json::from_value(json).unwrap();
        prop_assert_eq!(back, bytes);
    }
}

#[test]
fn json_forms() {
    let item = GenItem::ThinkingBlock {
        signature: Bytes::from(*b"foobar"),
        model: "m".into(),
    };
    let json = serde_json::to_value(&item).unwrap();
    assert_eq!(json["thinking_block"]["signature"], json!("Zm9vYmFy"));
    assert_eq!(serde_json::from_value::<GenItem>(json).unwrap(), item);
    assert!(serde_json::from_value::<Bytes>(json!("not base64!")).is_err());
    assert!(serde_json::from_value::<Bytes>(json!([1, 2, 255])).is_err());
}

#[test]
fn schema_is_base64_string() {
    let schema = serde_json::to_value(eva::_priv::schemars::schema_for!(Bytes)).unwrap();
    assert_eq!(schema["type"], "string");
    assert_eq!(schema["contentEncoding"], "base64");
}

#[test]
fn debug_is_short() {
    let big = Bytes::from(vec![0xab; 1 << 20]);
    let debug = format!("{big:?}");
    assert!(debug.len() < 64, "{debug}");
    assert!(debug.starts_with("Bytes[1048576: abab"), "{debug}");
}
