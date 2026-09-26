//! `NAMESPACES` повторяет таблицу операций `11 §3`.

use eva_core::{NAMESPACES, OpKind};

fn op(ns: &str, name: &str) -> OpKind {
    let (_, ops) = NAMESPACES
        .iter()
        .find(|(n, _)| *n == ns)
        .unwrap_or_else(|| panic!("no namespace {ns}"));
    ops.iter()
        .find(|op| op.name == name)
        .unwrap_or_else(|| panic!("no operation {ns}.{name}"))
        .kind
}

#[test]
fn table() {
    assert_eq!(NAMESPACES.len(), 20);
    assert_eq!(
        NAMESPACES.iter().map(|(_, ops)| ops.len()).sum::<usize>(),
        69
    );

    assert_eq!(op("chats", "move"), OpKind::Mutating);
    assert_eq!(op("chats", "get"), OpKind::Read);
    assert_eq!(op("turns", "attach"), OpKind::Stream);
    assert_eq!(op("blobs", "put"), OpKind::Upload);
    assert_eq!(op("blobs", "get"), OpKind::Stream);
    assert_eq!(op("hands_grants", "revoke"), OpKind::Mutating);
}

/// Glob-импорт прелюдии открывает методы неймспейсов у `Session`.
#[allow(dead_code)]
fn prelude_reaches_namespaces<S: eva_core::IsSession>(session: &mut eva_core::Session<S>) {
    use eva_core::prelude::*;

    let _ = session.surfaces().get();
    let _ = session.kv().list();
}
