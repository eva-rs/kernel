//! Кадр и ошибка RPC (`06 §1`). RPC симметричен: `request` шлют оба пира,
//! `response` всегда отвечает на чужой `id`.

use std::fmt;

use eva::{data, time::HumanDuration};

use crate::{
    errors::{Aux, HasCode},
    types::Bytes,
};

/// Payload кадра: msgpack (`to_vec_named`) `Args`/`Ok`/`Item` операции или метода.
pub type Payload = Bytes;

/// Кадр сессии; незнакомый вид кадра — `Unknown`, получатель его пропускает.
#[data]
#[serde(remote = "Frame")]
pub enum Frame {
    Request(Request),
    Response(Response),
    Notify(Notify),
    StreamItem(StreamItem),
    StreamHeartbeat(StreamHeartbeat),
    StreamEnd(StreamEnd),
    Cancel(Cancel),
    /// Вид кадра новее этого пира.
    #[serde(other)]
    Unknown,
}

tolerant!(Frame);

impl Frame {
    /// `id` вызова, к которому относится кадр; у `notify` его нет.
    pub const fn id(&self) -> Option<u64> {
        match self {
            Self::Request(Request { id, .. })
            | Self::Response(Response { id, .. })
            | Self::StreamItem(StreamItem { id, .. })
            | Self::StreamHeartbeat(StreamHeartbeat { id })
            | Self::StreamEnd(StreamEnd { id, .. })
            | Self::Cancel(Cancel { id }) => Some(*id),
            Self::Notify(_) | Self::Unknown => None,
        }
    }
}

/// Вызов; `id` уникален в пространстве отправителя в пределах сессии.
#[data]
pub struct Request {
    pub id: u64,
    /// `<namespace>.<op>` операции либо метод `wire::method`.
    pub method: String,
    pub params: Payload,
}

/// Ответ на не-потоковый вызов: ровно одно из `result`/`error`.
#[data]
pub struct Response {
    pub id: u64,
    pub result: Option<Payload>,
    pub error: Option<RpcError>,
}

impl Response {
    pub const fn ok(id: u64, result: Payload) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    pub const fn err(id: u64, error: RpcError) -> Self {
        Self {
            id,
            result: None,
            error: Some(error),
        }
    }

    /// Ответ без ровно одного из двух — ошибка пира, не успех.
    pub fn into_result(self) -> Result<Payload, RpcError> {
        match (self.result, self.error) {
            (Some(result), None) => Ok(result),
            (None, Some(error)) => Err(error),
            _ => Err(RpcError::new(
                ErrorCode::InvalidParams,
                "response must carry exactly one of result and error",
            )),
        }
    }
}

/// Сообщение без ответа.
#[data]
pub struct Notify {
    pub method: String,
    pub params: Payload,
}

/// Элемент потока на `Request::id`; `seq` с 0, без дыр.
///
/// У upload-потока (`blobs.put`) элемент — msgpack `Bytes` чанка.
#[data]
pub struct StreamItem {
    pub id: u64,
    pub seq: u64,
    pub item: Payload,
}

/// Поток жив; `seq` не занимает.
#[data(copy)]
pub struct StreamHeartbeat {
    pub id: u64,
}

/// Конец потока; `Response` потоку не шлётся.
#[data]
pub struct StreamEnd {
    pub id: u64,
    pub error: Option<RpcError>,
}

/// Просьба прекратить: ответ — `stream_end`/`response` с `cancelled`.
#[data(copy)]
pub struct Cancel {
    pub id: u64,
}

/// Ошибка вызова. `data` есть у ошибок операции (`Generic<E>`) и провайдера
/// (`ProviderError`); без неё — ошибка транспорта.
#[data(error)]
pub struct RpcError {
    pub code: ErrorCode,
    pub message: String,
    /// msgpack типизированной ошибки.
    pub data: Option<Payload>,
    pub retry_after: Option<HumanDuration>,
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl RpcError {
    pub fn new(code: ErrorCode, message: impl fmt::Display) -> Self {
        Self {
            code,
            message: message.to_string(),
            data: None,
            retry_after: None,
        }
    }

    /// Типизированная ошибка: код и текст — её, `data` — её msgpack.
    pub fn typed<E: HasCode + fmt::Display>(error: &E, data: Payload) -> Self {
        Self {
            code: error.code(),
            message: error.to_string(),
            data: Some(data),
            retry_after: error.retry_after(),
        }
    }

    pub fn unknown_method(method: &str) -> Self {
        Self::new(
            ErrorCode::UnknownMethod,
            format_args!("unknown method {method}"),
        )
    }

    pub fn invalid_params(reason: impl fmt::Display) -> Self {
        Self::new(ErrorCode::InvalidParams, reason)
    }

    pub fn cancelled() -> Self {
        Self::new(ErrorCode::Cancelled, "cancelled")
    }

    pub fn timeout() -> Self {
        Self::new(ErrorCode::Timeout, "timed out")
    }

    /// `Aux` по коду, когда `data` нет или она не разбирается (`11 §6`).
    pub fn into_aux(self) -> Aux {
        let Self { code, message, .. } = self;
        match code {
            ErrorCode::Timeout | ErrorCode::Cancelled | ErrorCode::Unavailable => {
                Aux::Unavailable(message)
            }
            ErrorCode::UnknownMethod | ErrorCode::ProtoVersion | ErrorCode::InvalidParams => {
                Aux::Invalid(message)
            }
            ErrorCode::Unauthorized | ErrorCode::Forbidden => Aux::Forbidden(message),
            ErrorCode::NotFound => Aux::NotFound(message),
            ErrorCode::Conflict => Aux::Conflict(message),
            ErrorCode::Quota => Aux::Quota(message),
            ErrorCode::Internal | ErrorCode::Unknown => Aux::Internal(message),
        }
    }
}

impl HasCode for RpcError {
    fn code(&self) -> ErrorCode {
        self.code
    }

    fn retry_after(&self) -> Option<HumanDuration> {
        self.retry_after
    }
}

/// Класс ошибки на проводе; решает сторона, знающая причину, по тексту не угадывается.
#[data(copy, ord, display(name))]
#[derive(Hash)]
#[serde(remote = "ErrorCode")]
pub enum ErrorCode {
    UnknownMethod,
    InvalidParams,
    Unauthorized,
    Forbidden,
    NotFound,
    Conflict,
    Timeout,
    Cancelled,
    Unavailable,
    ProtoVersion,
    Quota,
    Internal,
    /// Код новее этого пира.
    #[serde(other)]
    Unknown,
}

tolerant!(ErrorCode);
