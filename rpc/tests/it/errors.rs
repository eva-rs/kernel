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
    errors::sleep::AlreadyRunning => Conflict: errors::sleep::AlreadyRunning { run: "r".into() };
    errors::sleep::NotRunning => Conflict: errors::sleep::NotRunning { run: "r".into() };
    errors::sleep::NoProvider => Unavailable: errors::sleep::NoProvider { model: "missing".into() };
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
    ($($ns:ident: $Service:ident { $($op:ident => [$($dom:ident::$atom:ident),* $(,)?];)* })*) => {{
        let mut methods = Vec::new();
        $($(
            methods.push(format!(
                "{}.{}",
                stringify!($ns),
                stringify!($op).trim_start_matches("r#"),
            ));
            check_err::<requests::$Service::$op::Err>(vec![$((
                requests::$Service::$op::Err::from(<errors::$dom::$atom as Case>::sample()),
                <errors::$dom::$atom as Case>::CODE,
            )),*]);
        )*)*
        methods
    }};
}

#[test]
fn every_operation_error() {
    let mut methods = ops! {
        agents: Agents {
            list => [];
        }
        blobs: Blobs {
            put => [blobs::BadName, blobs::SizeMismatch, blobs::HashMismatch];
            get => [blobs::NotFound];
            stat => [blobs::NotFound];
        }
        chats: Chats {
            ingest => [];
            create => [];
            get => [chats::NotFound];
            list => [];
            update => [chats::NotFound, chats::KindMonotonic, chats::AgentTrust];
            r#move => [chats::NotFound, chats::TrustChange, chats::CrossSurface];
            delete => [chats::NotFound, chats::Undeletable];
            restore => [chats::NotFound, chats::NotDeleted];
            search => [];
            history_read => [chats::NotFound];
            compact => [chats::NotFound];
            notify => [];
        }
        cron: Cron {
            list => [];
            runs => [];
        }
        folders: Folders {
            create => [folders::NameTaken];
            update => [folders::NotFound, folders::Builtin, folders::NameTaken];
            delete => [folders::NotFound, folders::Builtin];
            list => [];
        }
        fs: Fs {
            read => [fs::NotFound, fs::IsDir];
            write => [fs::NotFound, fs::IsDir];
            list => [fs::NotFound, fs::NotDir];
            stat => [fs::NotFound];
            remove => [fs::NotFound];
        }
        hands_grants: HandsGrants {
            list => [];
            revoke => [hands_grants::NotFound];
        }
        inference: Inference {
            complete => [inference::Provider];
        }
        kv: Kv {
            get => [];
            set => [];
            del => [];
            list => [];
        }
        memory: Memory {
            list => [];
            get => [memory::NotFound];
            history => [memory::NotFound];
            restore => [memory::NotFound, memory::NotArchived, memory::Superseded];
        }
        models: Models {
            list => [];
        }
        modules: Modules {
            list => [];
            add => [modules::Taken];
            remove => [modules::ServiceNotFound];
            token_issue => [];
            token_revoke => [modules::NotFound];
        }
        notes: Notes {
            list => [chats::NotFound];
            put => [chats::NotFound];
            delete => [chats::NotFound];
            history => [chats::NotFound];
            snapshot => [chats::NotFound];
            restore => [chats::NotFound];
        }
        persons: Persons {
            resolve => [];
            list => [];
            rename => [persons::NotFound, persons::System];
            merge => [persons::NotFound, persons::System, persons::SelfMerge, persons::MasterMerge];
            whitelist => [persons::NotFound, persons::System, persons::MasterWhitelist];
        }
        sleep: Sleep {
            runs => [];
            phase_runs => [sleep::RunNotFound];
            status => [];
            start => [sleep::AlreadyRunning, sleep::NoProvider];
            cancel => [sleep::NotRunning];
        }
        sqlite: Sqlite {
            exec => [sqlite::Sql];
            query => [sqlite::Sql];
            tx => [sqlite::Sql];
        }
        stats: Stats {
            tools_usage => [];
            spend => [];
            spend_series => [];
            chat_spend => [chats::NotFound];
            health => [];
        }
        surfaces: Surfaces {
            create => [surfaces::Taken, surfaces::BadRules];
            get => [surfaces::NotFound];
            list => [];
            update => [surfaces::NotFound, surfaces::Builtin, surfaces::BadRules];
        }
        tools: Tools {
            catalog => [tools::PathNotFound];
        }
        turns: Turns {
            start => [chats::NotFound];
            attach => [chats::NotFound];
            append_silent => [chats::NotFound];
            delivered => [chats::NotFound, chats::MessageNotFound, chats::AlreadyDelivered];
            cancel => [chats::NotFound];
            respond => [turns::PendingNotFound, turns::AlreadyResolved];
        }
    };
    methods.sort();

    let mut expected: Vec<String> = NAMESPACES
        .iter()
        .flat_map(|(ns, ops)| ops.iter().map(move |op| format!("{ns}.{}", op.name)))
        .collect();
    expected.sort();

    assert_eq!(methods, expected, "one row per operation of NAMESPACES");
}
