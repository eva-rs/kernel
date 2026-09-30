//! Ошибки операций на проводе: у каждой операции `NAMESPACES` каждый вариант
//! `Err` переживает `Generic` → msgpack → `RpcError`, а `HasCode` совпадает
//! с таблицей `11 §6`.

use std::{collections::BTreeSet, fmt::Debug, time::Duration};

use eva::{_priv::schemars::JsonSchema, time::HumanDuration};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use eva_core::NAMESPACES;
use eva_protocol::{
    errors::{self, Generic, HasCode},
    requests,
    types::{
        ChatId, EventId, MemoryId,
        generate::{ProviderError, ProviderErrorKind},
    },
    wire::{ErrorCode, RpcError},
};

/// Атомарная ошибка: образец и ожидаемый код.
trait Case: Sized {
    const CODE: ErrorCode;
    fn sample() -> Self;
}

macro_rules! cases {
    ($($ty:ty => $code:ident: $sample:expr;)*) => {$(
        impl Case for $ty {
            const CODE: ErrorCode = ErrorCode::$code;
            fn sample() -> Self {
                $sample
            }
        }
    )*};
}

// `11 §6`: `*::NotFound` → not_found; Taken, Undeletable, TrustChange, AlreadyResolved → conflict; BadRules → invalid_params.
cases! {
    errors::blobs::NotFound => NotFound: errors::blobs::NotFound { id: "b1".into() };
    errors::blobs::BadName => InvalidParams: errors::blobs::BadName { name: "../x".into() };
    errors::blobs::SizeMismatch => InvalidParams: errors::blobs::SizeMismatch { declared: 3, actual: 4 };
    errors::blobs::HashMismatch => InvalidParams: errors::blobs::HashMismatch { declared: "a".into(), actual: "b".into() };
    errors::chats::NotFound => NotFound: errors::chats::NotFound { chat: ChatId(1).into() };
    errors::chats::Undeletable => Conflict: errors::chats::Undeletable { chat: ChatId(2) };
    errors::chats::TrustChange => Conflict: errors::chats::TrustChange { chat: ChatId(3), from: false, to: true };
    errors::chats::CrossSurface => InvalidParams: errors::chats::CrossSurface { chat: ChatId(4), surface: "x".into() };
    errors::chats::NotDeleted => Conflict: errors::chats::NotDeleted { chat: ChatId(5) };
    errors::chats::KindMonotonic => Conflict: errors::chats::KindMonotonic { chat: ChatId(6) };
    errors::chats::AgentTrust => Conflict: errors::chats::AgentTrust { chat: ChatId(7) };
    errors::chats::MessageNotFound => NotFound: errors::chats::MessageNotFound { chat: ChatId(8), message: EventId(9) };
    errors::chats::AlreadyDelivered => Conflict: errors::chats::AlreadyDelivered { message: EventId(10), external_id: "e".into() };
    errors::folders::NotFound => NotFound: errors::folders::NotFound { folder: 1.into() };
    errors::folders::NameTaken => Conflict: errors::folders::NameTaken { surface: "x".into(), name: "work".into() };
    errors::folders::Builtin => Conflict: errors::folders::Builtin { folder: 2.into() };
    errors::fs::NotFound => NotFound: errors::fs::NotFound { path: "a".into() };
    errors::fs::IsDir => InvalidParams: errors::fs::IsDir { path: "b".into() };
    errors::fs::NotDir => InvalidParams: errors::fs::NotDir { path: "c".into() };
    errors::hands_grants::NotFound => NotFound: errors::hands_grants::NotFound { id: 3.into() };
    errors::inference::Provider => Unavailable: errors::inference::Provider {
        error: ProviderError {
            kind: ProviderErrorKind::Transient,
            http_status: Some(529),
            raw: "overloaded".into(),
            retry_after: Some(HumanDuration(Duration::from_secs(7))),
        },
    };
    errors::memory::NotFound => NotFound: errors::memory::NotFound { id: MemoryId(1) };
    errors::memory::NotArchived => Conflict: errors::memory::NotArchived { id: MemoryId(2) };
    errors::memory::Superseded => Conflict: errors::memory::Superseded { id: MemoryId(3), current: MemoryId(4) };
    errors::modules::NotFound => NotFound: errors::modules::NotFound { module: "m".into() };
    errors::modules::Taken => Conflict: errors::modules::Taken { module: "m".into() };
    errors::modules::ServiceNotFound => NotFound: errors::modules::ServiceNotFound { module: "m".into() };
    errors::persons::NotFound => NotFound: errors::persons::NotFound { person: "p7".into() };
    errors::persons::System => InvalidParams: errors::persons::System { person: "p0".into() };
    errors::persons::SelfMerge => InvalidParams: errors::persons::SelfMerge { person: "p1".into() };
    errors::persons::MasterMerge => Conflict: errors::persons::MasterMerge { from: "p1".into(), into: "p2".into() };
    errors::persons::MasterWhitelist => Conflict: errors::persons::MasterWhitelist { person: "p1".into() };
    errors::sleep::RunNotFound => NotFound: errors::sleep::RunNotFound { run: "r".into() };
    errors::sqlite::Sql => InvalidParams: errors::sqlite::Sql { statement: 2, message: "syntax".into() };
    errors::surfaces::Taken => Conflict: errors::surfaces::Taken { surface: "x".into(), owner: Some("m".into()) };
    errors::surfaces::NotFound => NotFound: errors::surfaces::NotFound { surface: "y".into() };
    errors::surfaces::BadRules => InvalidParams: errors::surfaces::BadRules { line: 3 };
    errors::surfaces::Builtin => Conflict: errors::surfaces::Builtin { surface: "local".into(), field: "name".into() };
    errors::tools::PathNotFound => NotFound: errors::tools::PathNotFound { path: "a.b".into() };
    errors::turns::PendingNotFound => NotFound: errors::turns::PendingNotFound { pending: "q".into() };
    errors::turns::AlreadyResolved => Conflict: errors::turns::AlreadyResolved { pending: "q".into() };
}

fn msgpack<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T) -> T {
    let bytes = rmp_serde::to_vec_named(value).expect("encode");
    let back: T = rmp_serde::from_slice(&bytes)
        .unwrap_or_else(|e| panic!("{}: {e}", std::any::type_name::<T>()));
    assert_eq!(&back, value);
    back
}

/// Имена вариантов по схеме: полнота таблицы без ручного счёта.
fn schema_variants<E: JsonSchema>() -> BTreeSet<String> {
    let schema = serde_json::to_value(eva::_priv::schemars::schema_for!(E)).expect("schema");
    schema
        .get("oneOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|variant| variant["required"][0].as_str().map(str::to_owned))
        .collect()
}

fn check_err<E>(cases: Vec<(E, ErrorCode)>)
where
    E: HasCode + Serialize + DeserializeOwned + PartialEq + Debug + JsonSchema + std::fmt::Display,
{
    let name = std::any::type_name::<E>();
    let mut covered = BTreeSet::new();

    for (err, code) in cases {
        let json = serde_json::to_value(&err).expect("json");
        let variant = json
            .as_object()
            .and_then(|map| map.keys().next().cloned())
            .unwrap_or_else(|| panic!("{name}: {json}"));
        covered.insert(variant);

        let generic = Generic::<E>::Spec(err);
        let back = msgpack(&generic);
        assert!(matches!(back, Generic::Spec(_)), "{name}: decoded as Aux");
        assert_eq!(generic.code(), code, "{name}: {generic}");

        let rpc = RpcError::typed(
            &generic,
            rmp_serde::to_vec_named(&generic).expect("encode").into(),
        );
        assert_eq!(rpc.code, code);
        assert_eq!(rpc.message, generic.to_string());
        assert_eq!(rpc.retry_after, generic.retry_after());
        let data = rpc.data.expect("typed error carries data");
        let decoded: Generic<E> = rmp_serde::from_slice(&data).expect("data decodes");
        assert_eq!(decoded, generic);
    }

    assert_eq!(
        covered,
        schema_variants::<E>(),
        "{name}: every variant needs a case"
    );
}

/// Проверяет строки таблицы и возвращает их RPC-методы `ns.op`.
macro_rules! ops {
    ($($ns:ident::$op:ident => [$($dom:ident::$atom:ident),* $(,)?];)*) => {{
        let mut methods = Vec::new();
        $(
            methods.push(format!(
                "{}.{}",
                stringify!($ns),
                stringify!($op).trim_start_matches("r#"),
            ));
            check_err::<requests::$ns::$op::Err>(vec![$((
                requests::$ns::$op::Err::from(<errors::$dom::$atom as Case>::sample()),
                <errors::$dom::$atom as Case>::CODE,
            )),*]);
        )*
        methods
    }};
}

#[test]
fn every_operation_error() {
    let mut methods = ops! {
        agents::list => [];
        blobs::put => [blobs::BadName, blobs::SizeMismatch, blobs::HashMismatch];
        blobs::get => [blobs::NotFound];
        blobs::stat => [blobs::NotFound];
        chats::ingest => [];
        chats::create => [];
        chats::get => [chats::NotFound];
        chats::list => [];
        chats::update => [chats::NotFound, chats::KindMonotonic, chats::AgentTrust];
        chats::r#move => [chats::NotFound, chats::TrustChange, chats::CrossSurface];
        chats::delete => [chats::NotFound, chats::Undeletable];
        chats::restore => [chats::NotFound, chats::NotDeleted];
        chats::search => [];
        chats::history_read => [chats::NotFound];
        chats::compact => [chats::NotFound];
        chats::notify => [];
        cron::list => [];
        cron::runs => [];
        folders::create => [folders::NameTaken];
        folders::update => [folders::NotFound, folders::Builtin, folders::NameTaken];
        folders::delete => [folders::NotFound, folders::Builtin];
        folders::list => [];
        fs::read => [fs::NotFound, fs::IsDir];
        fs::write => [fs::NotFound, fs::IsDir];
        fs::list => [fs::NotFound, fs::NotDir];
        fs::stat => [fs::NotFound];
        fs::remove => [fs::NotFound];
        hands_grants::list => [];
        hands_grants::revoke => [hands_grants::NotFound];
        inference::complete => [inference::Provider];
        kv::get => [];
        kv::set => [];
        kv::del => [];
        kv::list => [];
        memory::list => [];
        memory::get => [memory::NotFound];
        memory::history => [memory::NotFound];
        memory::restore => [memory::NotFound, memory::NotArchived, memory::Superseded];
        models::list => [];
        modules::list => [];
        modules::add => [modules::Taken];
        modules::remove => [modules::ServiceNotFound];
        modules::token_issue => [];
        modules::token_revoke => [modules::NotFound];
        notes::list => [chats::NotFound];
        persons::resolve => [];
        persons::list => [];
        persons::rename => [persons::NotFound, persons::System];
        persons::merge => [persons::NotFound, persons::System, persons::SelfMerge, persons::MasterMerge];
        persons::whitelist => [persons::NotFound, persons::System, persons::MasterWhitelist];
        sleep::runs => [];
        sleep::phase_runs => [sleep::RunNotFound];
        sqlite::exec => [sqlite::Sql];
        sqlite::query => [sqlite::Sql];
        sqlite::tx => [sqlite::Sql];
        stats::tools_usage => [];
        stats::spend => [];
        stats::spend_series => [];
        stats::chat_spend => [chats::NotFound];
        stats::health => [];
        surfaces::create => [surfaces::Taken, surfaces::BadRules];
        surfaces::get => [surfaces::NotFound];
        surfaces::list => [];
        surfaces::update => [surfaces::NotFound, surfaces::Builtin, surfaces::BadRules];
        tools::catalog => [tools::PathNotFound];
        turns::start => [chats::NotFound];
        turns::attach => [chats::NotFound];
        turns::append_silent => [chats::NotFound];
        turns::delivered => [chats::NotFound, chats::MessageNotFound, chats::AlreadyDelivered];
        turns::cancel => [chats::NotFound];
        turns::respond => [turns::PendingNotFound, turns::AlreadyResolved];
    };
    methods.sort();

    let mut expected: Vec<String> = NAMESPACES
        .iter()
        .flat_map(|(ns, ops)| ops.iter().map(move |op| format!("{ns}.{}", op.name)))
        .collect();
    expected.sort();

    assert_eq!(methods, expected, "one row per operation of NAMESPACES");
}
