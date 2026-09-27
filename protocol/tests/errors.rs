//! Ошибки на проводе: каждый вариант `Aux` переживает `Generic` → msgpack →
//! `Generic` и `Generic` → `RpcError` → `Generic`, а `HasCode` совпадает с
//! таблицей `11 §6`. Таблица `Err` всех операций — в `rpc/tests/it/errors.rs`,
//! где видны `NAMESPACES`.

mod common;

use std::{fmt::Debug, time::Duration};

use eva::time::HumanDuration;
use serde::{Serialize, de::DeserializeOwned};

use eva_protocol::{
    errors::{self, Aux, Generic, HasCode},
    requests,
    types::{
        ChatId,
        generate::{ProviderError, ProviderErrorKind},
    },
    wire::{ErrorCode, RpcError},
};

fn msgpack<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T) -> T {
    let bytes = rmp_serde::to_vec_named(value).expect("encode");
    let back: T = rmp_serde::from_slice(&bytes)
        .unwrap_or_else(|e| panic!("{}: {e}", std::any::type_name::<T>()));
    assert_eq!(&back, value);
    back
}

#[test]
fn every_aux_variant() {
    type E = Generic<requests::chats::delete::Err>;

    let cases = [
        (Aux::Db("locked".into()), ErrorCode::Internal),
        (Aux::Forbidden("owner only".into()), ErrorCode::Forbidden),
        (Aux::NotFound("folder".into()), ErrorCode::NotFound),
        (Aux::Unavailable("hands".into()), ErrorCode::Unavailable),
        (Aux::Invalid("limit".into()), ErrorCode::InvalidParams),
        (Aux::Quota("rows".into()), ErrorCode::Quota),
        (Aux::Conflict("live turn".into()), ErrorCode::Conflict),
        (Aux::Internal("bug".into()), ErrorCode::Internal),
    ];
    common::assert_every_variant(cases.iter().map(|(aux, _)| aux));

    for (aux, code) in cases {
        let generic = E::Aux(aux.clone());
        let back = msgpack(&generic);
        assert_eq!(back, E::Aux(aux), "Aux must win the untagged race");
        assert_eq!(generic.code(), code);
        assert_eq!(generic.retry_after(), None);

        // `11 §8`: так ошибку несёт кадр — код, текст и msgpack в `data`.
        let data = rmp_serde::to_vec_named(&generic).expect("encode");
        let rpc = msgpack(&RpcError::typed(&generic, data.into()));
        assert_eq!(rpc.code, code, "{generic}");
        assert_eq!(rpc.message, generic.to_string());
        assert_eq!(rpc.retry_after, None);
        let data = rpc.data.expect("typed error carries data");
        let back: E = rmp_serde::from_slice(&data).expect("data decodes");
        assert_eq!(back, generic);
    }
}

#[test]
fn provider_error_kinds() {
    let cases = [
        (ProviderErrorKind::Transient, ErrorCode::Unavailable),
        (ProviderErrorKind::Auth, ErrorCode::Unavailable),
        (ProviderErrorKind::NotFound, ErrorCode::NotFound),
        (ProviderErrorKind::Quota, ErrorCode::Quota),
        (ProviderErrorKind::Overflow, ErrorCode::InvalidParams),
        (ProviderErrorKind::ContentFilter, ErrorCode::InvalidParams),
        (ProviderErrorKind::Invalid, ErrorCode::InvalidParams),
        (ProviderErrorKind::Other, ErrorCode::Internal),
    ];
    for (kind, code) in cases {
        let error = ProviderError {
            kind,
            http_status: None,
            raw: "raw".into(),
            retry_after: Some(HumanDuration(Duration::from_secs(3))),
        };
        msgpack(&error);
        assert_eq!(error.code(), code, "{kind}");
        assert_eq!(error.retry_after(), error.retry_after);

        let op = errors::inference::Provider { error };
        let generic = Generic::<requests::inference::complete::Err>::Spec(op.into());
        assert_eq!(generic.code(), code);
        assert_eq!(
            generic.retry_after(),
            Some(HumanDuration(Duration::from_secs(3)))
        );
    }
}

/// Без разборчивой `data` клиент восстанавливает `Aux` по коду.
#[test]
fn transport_codes_to_aux() {
    let cases = [
        (ErrorCode::Timeout, Aux::Unavailable("m".into())),
        (ErrorCode::Cancelled, Aux::Unavailable("m".into())),
        (ErrorCode::Unavailable, Aux::Unavailable("m".into())),
        (ErrorCode::UnknownMethod, Aux::Invalid("m".into())),
        (ErrorCode::ProtoVersion, Aux::Invalid("m".into())),
        (ErrorCode::InvalidParams, Aux::Invalid("m".into())),
        (ErrorCode::Unauthorized, Aux::Forbidden("m".into())),
        (ErrorCode::Forbidden, Aux::Forbidden("m".into())),
        (ErrorCode::NotFound, Aux::NotFound("m".into())),
        (ErrorCode::Conflict, Aux::Conflict("m".into())),
        (ErrorCode::Quota, Aux::Quota("m".into())),
        (ErrorCode::Internal, Aux::Internal("m".into())),
        (ErrorCode::Unknown, Aux::Internal("m".into())),
    ];
    for (code, aux) in cases {
        assert_eq!(RpcError::new(code, "m").into_aux(), aux, "{code}");
    }
}

/// Селектор-адрес внутри untagged `Generic`: `Spec`, а не проигравший гонку `Aux`.
#[test]
fn not_found_by_alias() {
    let alias = errors::chats::NotFound {
        chat: requests::chats::Selector::Alias {
            surface: "x".into(),
            external_id: "ext-1".into(),
        },
    };
    let back = msgpack(&Generic::<requests::chats::get::Err>::Spec(
        alias.clone().into(),
    ));
    assert_eq!(back.code(), ErrorCode::NotFound);
    assert!(back.to_string().contains("x:ext-1"), "{back}");
    let back = msgpack(&Generic::<requests::chats::r#move::Err>::Spec(alias.into()));
    assert!(matches!(back, Generic::Spec(_)));

    let by_id = msgpack(&Generic::<requests::turns::start::Err>::Spec(
        errors::chats::NotFound {
            chat: ChatId(4).into(),
        }
        .into(),
    ));
    assert!(matches!(by_id, Generic::Spec(_)));
}
