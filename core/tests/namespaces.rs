//! `NAMESPACES` и `prelude` из списка операций.

use eva_core::{NAMESPACES, Op, OpKind};

/// RPC-метод — имя модуля без `r#`: `chats.move`, не `chats.r#move`.
#[test]
fn raw_identifiers_are_plain_on_the_wire() {
    let raw: Vec<String> = NAMESPACES
        .iter()
        .flat_map(|(ns, ops)| ops.iter().map(move |op| format!("{ns}.{}", op.name)))
        .filter(|method| method.contains("r#"))
        .collect();
    assert!(raw.is_empty(), "{raw:?}");

    let chats = NAMESPACES
        .iter()
        .find_map(|(ns, ops)| (*ns == "chats").then_some(*ops))
        .expect("chats namespace");
    assert!(chats.contains(&Op {
        name: "move",
        kind: OpKind::Mutating,
    }));
}

/// Glob-импорт прелюдии открывает методы неймспейсов у `Session`.
#[allow(dead_code)]
fn prelude_reaches_namespaces<S: eva_core::IsSession>(session: &mut eva_core::Session<S>) {
    use eva_core::prelude::*;

    let _ = session.surfaces().get();
    let _ = session.kv().list();
    let _ = session.turns().attach();
    let _ = session.blobs().put();
}
