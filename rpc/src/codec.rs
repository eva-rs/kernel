//! Кодировка провода: msgpack с именами полей (`rmp_serde::to_vec_named`).
//!
//! Позиционная форма ломает untagged `Generic` и пропуск незнакомых полей
//! пира новее, поэтому другой кодировки у крейта нет.

use std::fmt;

use eva::data;
use serde::{Serialize, de::DeserializeOwned};

use eva_protocol::{
    errors::{Aux, HasCode},
    types::Bytes,
    wire::{ErrorCode, Frame, RpcError, frame::Payload},
};

#[data(error)]
pub enum CodecError {
    #[display("encode: {_0}")]
    Encode(String),
    #[display("decode: {_0}")]
    Decode(String),
}

pub fn encode<T: Serialize + ?Sized>(value: &T) -> Result<Payload, CodecError> {
    rmp_serde::to_vec_named(value)
        .map(Bytes)
        .map_err(|e| CodecError::Encode(e.to_string()))
}

pub fn decode<T: DeserializeOwned>(raw: &[u8]) -> Result<T, CodecError> {
    rmp_serde::from_slice(raw).map_err(|e| CodecError::Decode(e.to_string()))
}

pub fn encode_frame(frame: &Frame) -> Result<bytes::Bytes, CodecError> {
    rmp_serde::to_vec_named(frame)
        .map(bytes::Bytes::from)
        .map_err(|e| CodecError::Encode(e.to_string()))
}

pub fn decode_frame(raw: &[u8]) -> Result<Frame, CodecError> {
    decode(raw)
}

/// Аргументы, которые получатель не понял: `invalid_params`.
pub fn bad_params(e: impl fmt::Display) -> RpcError {
    RpcError::invalid_params(format_args!("invalid params: {e}"))
}

/// Результат, который не разобрал вызывающий: пир говорит на другой версии.
pub fn bad_result(e: impl fmt::Display) -> RpcError {
    RpcError::invalid_params(format_args!("malformed result: {e}"))
}

/// Типизированная ошибка: код и текст — её, `data` — её msgpack.
pub fn typed<E: Serialize + HasCode + fmt::Display>(error: &E) -> RpcError {
    match encode(error) {
        Ok(data) => RpcError::typed(error, data),
        Err(e) => RpcError::new(ErrorCode::Internal, format_args!("{error} ({e})")),
    }
}

/// `Aux` с `data`: получатель разберёт её как `Generic::Aux` любой операции.
pub fn aux(error: &Aux) -> RpcError {
    typed(error)
}

/// Обратно из провода: `data`, если она разбирается, иначе класс по коду.
pub fn into_aux(error: RpcError) -> Aux {
    if let Some(aux) = error.data.as_ref().and_then(|d| decode::<Aux>(d).ok()) {
        return aux;
    }
    error.into_aux()
}

pub(crate) fn unavailable(what: impl fmt::Display) -> RpcError {
    RpcError::new(ErrorCode::Unavailable, what)
}

/// Строка пира для логов: экранирована и обрезана, чтобы чужой текст не
/// подделал строки лога и не раздул его.
pub(crate) fn shown(text: &str) -> String {
    const MAX_CHARS: usize = 64;
    match text.char_indices().nth(MAX_CHARS) {
        Some((cut, _)) => format!("{:?}…", &text[..cut]),
        None => format!("{text:?}"),
    }
}
