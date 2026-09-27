//! `NAMESPACES` повторяет таблицу операций `11 §3`.

use eva_core::{NAMESPACES, OpKind};

use OpKind::{Mutating, Read, Stream, Upload};

/// Форма шага по `11 §3`: `Read` SDK повторяет после реконнекта с новым id,
/// и мутация с этой меткой исполнилась бы дважды.
const TABLE: &[(&str, OpKind)] = &[
    ("surfaces.create", Mutating),
    ("surfaces.get", Read),
    ("surfaces.list", Read),
    ("surfaces.update", Mutating),
    ("folders.create", Mutating),
    ("folders.update", Mutating),
    ("folders.delete", Mutating),
    ("folders.list", Read),
    ("chats.ingest", Mutating),
    ("chats.create", Mutating),
    ("chats.get", Read),
    ("chats.list", Read),
    ("chats.update", Mutating),
    ("chats.move", Mutating),
    ("chats.delete", Mutating),
    ("chats.restore", Mutating),
    ("chats.search", Read),
    ("chats.history_read", Read),
    ("chats.compact", Mutating),
    ("chats.notify", Mutating),
    ("turns.start", Mutating),
    ("turns.attach", Stream),
    ("turns.append_silent", Mutating),
    ("turns.delivered", Mutating),
    ("turns.cancel", Mutating),
    ("turns.respond", Mutating),
    // Заводит неизвестного актора.
    ("persons.resolve", Mutating),
    ("persons.list", Read),
    ("persons.rename", Mutating),
    ("persons.merge", Mutating),
    ("persons.whitelist", Mutating),
    ("memory.list", Read),
    ("memory.get", Read),
    ("memory.history", Read),
    ("memory.restore", Mutating),
    ("notes.list", Read),
    ("tools.catalog", Read),
    ("models.list", Read),
    ("cron.list", Read),
    ("cron.runs", Read),
    ("sleep.runs", Read),
    ("sleep.phase_runs", Read),
    ("agents.list", Read),
    ("stats.tools_usage", Read),
    ("stats.spend", Read),
    ("stats.spend_series", Read),
    ("stats.chat_spend", Read),
    ("stats.health", Read),
    ("modules.list", Read),
    ("modules.token_issue", Mutating),
    ("modules.token_revoke", Mutating),
    ("hands_grants.list", Read),
    ("hands_grants.revoke", Mutating),
    ("blobs.put", Upload),
    ("blobs.get", Stream),
    ("blobs.stat", Read),
    // Пишет spend: повтор вызова оплатился бы дважды.
    ("inference.complete", Mutating),
    ("fs.read", Read),
    ("fs.write", Mutating),
    ("fs.list", Read),
    ("fs.stat", Read),
    ("fs.remove", Mutating),
    ("sqlite.exec", Mutating),
    // `06 §5`: только `sqlite3_stmt_readonly`-операторы.
    ("sqlite.query", Read),
    ("sqlite.tx", Mutating),
    ("kv.get", Read),
    ("kv.set", Mutating),
    ("kv.del", Mutating),
    ("kv.list", Read),
];

#[test]
fn table() {
    let library: Vec<(String, OpKind)> = NAMESPACES
        .iter()
        .flat_map(|(ns, ops)| {
            ops.iter()
                .map(move |op| (format!("{ns}.{}", op.name), op.kind))
        })
        .collect();
    let design: Vec<(String, OpKind)> = TABLE
        .iter()
        .map(|(method, kind)| ((*method).to_owned(), *kind))
        .collect();

    let design_only: Vec<_> = design.iter().filter(|r| !library.contains(r)).collect();
    let library_only: Vec<_> = library.iter().filter(|r| !design.contains(r)).collect();
    assert!(
        design_only.is_empty() && library_only.is_empty(),
        "design: {design_only:?}, library: {library_only:?}"
    );
    assert_eq!(library, design, "operation order differs from 11 §3");
}

/// Glob-импорт прелюдии открывает методы неймспейсов у `Session`.
#[allow(dead_code)]
fn prelude_reaches_namespaces<S: eva_core::IsSession>(session: &mut eva_core::Session<S>) {
    use eva_core::prelude::*;

    let _ = session.surfaces().get();
    let _ = session.kv().list();
}
