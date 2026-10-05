//! Конверт JSON-RPC 2.0 в обоих кодеках: `{jsonrpc: "2.0", id, method,
//! params}` → `{jsonrpc, id, result}` либо `{jsonrpc, id, error: {code,
//! message, data}}`. `params` остаются байтами кодека до выбора метода и
//! разбираются сразу в `Args`; `data` ошибки — `{code: ErrorCode,
//! retry_after?, error?: Generic<E>}`, где `error` — тот же объект, что
//! `RpcError.data` на проводе.

use std::{borrow::Cow, fmt};

use eva::time::HumanDuration;
use rmp::{Marker, decode::DecodeStringError};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, DeserializeOwned},
};
use serde_json::value::RawValue;

use eva_protocol::{
    errors::HasCode,
    wire::{ErrorCode, RpcError},
};

use super::format::Format;

/// Коды `error.code`: стандартные JSON-RPC и свои, по одному на `ErrorCode`
/// без стандартного аналога. Класс ошибки клиент читает из `data.code`.
pub mod code {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;

    pub const UNAUTHORIZED: i32 = -32001;
    pub const FORBIDDEN: i32 = -32003;
    pub const NOT_FOUND: i32 = -32004;
    pub const TIMEOUT: i32 = -32008;
    pub const CONFLICT: i32 = -32009;
    pub const CANCELLED: i32 = -32010;
    pub const UNAVAILABLE: i32 = -32011;
    pub const PROTO_VERSION: i32 = -32012;
    pub const QUOTA: i32 = -32013;
}

pub const fn code_of(code: ErrorCode) -> i32 {
    match code {
        ErrorCode::UnknownMethod => code::METHOD_NOT_FOUND,
        ErrorCode::InvalidParams => code::INVALID_PARAMS,
        ErrorCode::Unauthorized => code::UNAUTHORIZED,
        ErrorCode::Forbidden => code::FORBIDDEN,
        ErrorCode::NotFound => code::NOT_FOUND,
        ErrorCode::Conflict => code::CONFLICT,
        ErrorCode::Timeout => code::TIMEOUT,
        ErrorCode::Cancelled => code::CANCELLED,
        ErrorCode::Unavailable => code::UNAVAILABLE,
        ErrorCode::ProtoVersion => code::PROTO_VERSION,
        ErrorCode::Quota => code::QUOTA,
        ErrorCode::Internal | ErrorCode::Unknown => code::INTERNAL_ERROR,
    }
}

/// Id запроса; возвращается как пришёл.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Id {
    Null,
    Int(i64),
    Uint(u64),
    Str(String),
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl de::Visitor<'_> for Visitor {
            type Value = Id;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string, an integer or null")
            }

            fn visit_unit<E>(self) -> Result<Id, E> {
                Ok(Id::Null)
            }

            fn visit_none<E>(self) -> Result<Id, E> {
                Ok(Id::Null)
            }

            fn visit_i64<E>(self, v: i64) -> Result<Id, E> {
                Ok(Id::Int(v))
            }

            fn visit_u64<E>(self, v: u64) -> Result<Id, E> {
                Ok(Id::Uint(v))
            }

            fn visit_str<E>(self, v: &str) -> Result<Id, E> {
                Ok(Id::Str(v.to_owned()))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// `params` в кодеке запроса: члены конверта не упорядочены, поэтому они
/// разбираются уже под известный метод.
pub enum Params {
    Absent,
    Json(Box<RawValue>),
    Msgpack(Vec<u8>),
    /// Позиционные или скалярные params; каждая операция берёт объект.
    NotAnObject,
}

impl Params {
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T, String> {
        match self {
            Self::Absent => Format::Json.load(b"{}"),
            Self::Json(raw) => Format::Json.load(raw.get().as_bytes()),
            Self::Msgpack(bytes) => Format::Msgpack.load(bytes),
            Self::NotAnObject => Err(String::from("params must be an object")),
        }
    }
}

pub struct Request<'a> {
    /// `None` — уведомление: исполняется, ответа не ждёт.
    pub id: Option<Id>,
    pub method: Cow<'a, str>,
    pub params: Params,
}

/// Тело не стало запросом; ответ с `id: null`.
#[derive(Debug)]
pub struct Rejection {
    pub code: i32,
    pub message: String,
}

impl Rejection {
    pub(super) fn parse(message: impl fmt::Display) -> Self {
        Self {
            code: code::PARSE_ERROR,
            message: format!("parse error: {message}"),
        }
    }

    pub(super) fn invalid(message: impl fmt::Display) -> Self {
        Self {
            code: code::INVALID_REQUEST,
            message: format!("invalid request: {message}"),
        }
    }

    pub fn answer(&self, format: Format) -> Vec<u8> {
        failure(
            format,
            &Id::Null,
            self.code,
            &self.message,
            &ErrorData::<()>::plain(ErrorCode::InvalidParams),
        )
    }
}

/// Один запрос либо batch, чьи элементы отвергаются по одному.
pub enum Message<'a> {
    Single(Request<'a>),
    Batch(Vec<Result<Request<'a>, Rejection>>),
}

/// Batch длиннее `max_batch` отвергается целиком до разбора элементов.
pub fn parse(format: Format, body: &[u8], max_batch: usize) -> Result<Message<'_>, Rejection> {
    match format {
        Format::Json => parse_json(body, max_batch),
        Format::Msgpack => parse_msgpack(body, max_batch),
    }
}

fn check_batch_len(len: usize, max_batch: usize) -> Result<(), Rejection> {
    match len {
        0 => Err(Rejection::invalid("empty batch")),
        len if len > max_batch => Err(Rejection::invalid(format_args!(
            "batch of {len} requests exceeds max_batch of {max_batch}"
        ))),
        _ => Ok(()),
    }
}

fn check_version(version: &str) -> Result<(), Rejection> {
    if version == "2.0" {
        Ok(())
    } else {
        Err(Rejection::invalid(format_args!(
            "unsupported jsonrpc version {version:?}"
        )))
    }
}

fn parse_json(body: &[u8], max_batch: usize) -> Result<Message<'_>, Rejection> {
    let raw: &RawValue = serde_json::from_slice(body).map_err(Rejection::parse)?;
    if raw.get().as_bytes().first() != Some(&b'[') {
        return json_request(raw).map(Message::Single);
    }

    let elements: Vec<&RawValue> = serde_json::from_str(raw.get()).map_err(Rejection::parse)?;
    check_batch_len(elements.len(), max_batch)?;
    Ok(Message::Batch(
        elements.into_iter().map(json_request).collect(),
    ))
}

fn json_request(raw: &RawValue) -> Result<Request<'_>, Rejection> {
    #[derive(Deserialize)]
    struct Envelope<'a> {
        #[serde(borrow)]
        jsonrpc: Cow<'a, str>,
        #[serde(default, deserialize_with = "present")]
        id: Option<Id>,
        #[serde(borrow)]
        method: Cow<'a, str>,
        #[serde(default, borrow)]
        params: Option<&'a RawValue>,
    }

    fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Id>, D::Error> {
        Id::deserialize(deserializer).map(Some)
    }

    if raw.get().as_bytes().first() != Some(&b'{') {
        return Err(Rejection::invalid("request must be an object"));
    }

    let envelope: Envelope<'_> = serde_json::from_str(raw.get()).map_err(Rejection::invalid)?;
    check_version(&envelope.jsonrpc)?;

    let params = match envelope.params {
        None => Params::Absent,
        Some(raw) => match raw.get().as_bytes().first() {
            Some(b'{') => Params::Json(raw.to_owned()),
            Some(b'n') => Params::Absent,
            _ => Params::NotAnObject,
        },
    };

    Ok(Request {
        id: envelope.id,
        method: envelope.method,
        params,
    })
}

fn parse_msgpack(body: &[u8], max_batch: usize) -> Result<Message<'_>, Rejection> {
    let len = value_len(body).map_err(|_| Rejection::parse("malformed msgpack"))?;
    if len != body.len() {
        return Err(Rejection::parse("trailing data after the request"));
    }
    let Ok((Head::Array(count), mut rest)) = head(body) else {
        return msgpack_request(body).map(Message::Single);
    };
    check_batch_len(count, max_batch)?;

    let requests = (0..count)
        .map(|_| {
            // Не падает: тело целиком прошло `value_len`.
            let len = value_len(rest).unwrap_or(rest.len());
            let (raw, tail) = rest.split_at(len);
            rest = tail;
            msgpack_request(raw)
        })
        .collect();
    Ok(Message::Batch(requests))
}

/// `raw` — ровно одно корректное значение.
fn msgpack_request(raw: &[u8]) -> Result<Request<'_>, Rejection> {
    fn string(raw: &[u8]) -> Option<&str> {
        match rmp::decode::read_str_from_slice(raw) {
            Ok((s, [])) => Some(s),
            _ => None,
        }
    }

    let Ok((Head::Map(entries), mut rest)) = head(raw) else {
        return Err(Rejection::invalid("request must be a map"));
    };

    let mut version = None;
    let mut id = None;
    let mut method = None;
    let mut params = Params::Absent;
    let mut seen = [false; 4];

    for _ in 0..entries {
        let (key, tail) = rmp::decode::read_str_from_slice(rest).map_err(|e| match e {
            DecodeStringError::TypeMismatch(_) => Rejection::invalid("map keys must be strings"),
            e => Rejection::parse(e),
        })?;
        let len = value_len(tail).map_err(|_| Rejection::parse("malformed msgpack"))?;
        let (raw, tail) = tail.split_at(len);
        rest = tail;

        if let Some(member) = ["jsonrpc", "id", "method", "params"]
            .iter()
            .position(|m| *m == key)
            && std::mem::replace(&mut seen[member], true)
        {
            return Err(Rejection::invalid(format_args!("duplicate member {key:?}")));
        }

        match key {
            "jsonrpc" => version = string(raw),
            "id" => {
                let parsed = rmp_serde::from_slice::<Id>(raw)
                    .map_err(|_| Rejection::invalid("id must be a string, an integer or nil"))?;
                id = Some(parsed);
            }
            "method" => {
                let name =
                    string(raw).ok_or_else(|| Rejection::invalid("method must be a string"))?;
                method = Some(Cow::Borrowed(name));
            }
            "params" => {
                params = match head(raw) {
                    Ok((Head::Map(_), _)) => Params::Msgpack(raw.to_vec()),
                    Ok((Head::Nil, _)) => Params::Absent,
                    _ => Params::NotAnObject,
                };
            }
            _ => {}
        }
    }

    check_version(version.unwrap_or_default())?;
    let method = method.ok_or_else(|| Rejection::invalid("missing method"))?;

    Ok(Request { id, method, params })
}

/// Вложенность msgpack, как у `serde_json` по умолчанию.
const MAX_DEPTH: usize = 128;

#[derive(Debug, Clone, Copy)]
struct Malformed;

enum Head {
    Array(usize),
    Map(usize),
    Nil,
    Other,
}

/// Заголовок значения: контейнер с числом элементов и байты после него.
fn head(buf: &[u8]) -> Result<(Head, &[u8]), Malformed> {
    let mut rd = Reader(buf);
    let head = match rd.marker()? {
        Marker::FixArray(n) => Head::Array(usize::from(n)),
        Marker::Array16 => Head::Array(rd.len(2)?),
        Marker::Array32 => Head::Array(rd.len(4)?),
        Marker::FixMap(n) => Head::Map(usize::from(n)),
        Marker::Map16 => Head::Map(rd.len(2)?),
        Marker::Map32 => Head::Map(rd.len(4)?),
        Marker::Null => Head::Nil,
        _ => Head::Other,
    };
    Ok((head, rd.0))
}

/// Длина в байтах значения msgpack в начале `buf`; любой вид значения,
/// включая `bin` и `ext`.
fn value_len(buf: &[u8]) -> Result<usize, Malformed> {
    let mut rd = Reader(buf);
    rd.skip(MAX_DEPTH)?;
    Ok(buf.len() - rd.0.len())
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn marker(&mut self) -> Result<Marker, Malformed> {
        let (&first, rest) = self.0.split_first().ok_or(Malformed)?;
        self.0 = rest;
        Ok(Marker::from_u8(first))
    }

    fn advance(&mut self, n: usize) -> Result<(), Malformed> {
        self.0 = self.0.get(n..).ok_or(Malformed)?;
        Ok(())
    }

    /// Беззнаковая длина big-endian в `bytes` байтах.
    fn len(&mut self, bytes: usize) -> Result<usize, Malformed> {
        let raw = self.0.get(..bytes).ok_or(Malformed)?;
        let len = raw
            .iter()
            .fold(0_usize, |acc, &b| acc << 8 | usize::from(b));
        self.advance(bytes)?;
        Ok(len)
    }

    fn skip(&mut self, depth: usize) -> Result<(), Malformed> {
        use Marker::*;

        let (items, pairs) = match self.marker()? {
            FixPos(_) | FixNeg(_) | Null | True | False => return Ok(()),
            Reserved => return Err(Malformed),
            U8 | I8 => return self.advance(1),
            U16 | I16 => return self.advance(2),
            U32 | I32 | F32 => return self.advance(4),
            U64 | I64 | F64 => return self.advance(8),
            FixStr(n) => return self.advance(usize::from(n)),
            Str8 | Bin8 => {
                let n = self.len(1)?;
                return self.advance(n);
            }
            Str16 | Bin16 => {
                let n = self.len(2)?;
                return self.advance(n);
            }
            Str32 | Bin32 => {
                let n = self.len(4)?;
                return self.advance(n);
            }
            FixExt1 => return self.advance(2),
            FixExt2 => return self.advance(3),
            FixExt4 => return self.advance(5),
            FixExt8 => return self.advance(9),
            FixExt16 => return self.advance(17),
            Ext8 => {
                let n = self.len(1)?;
                return self.advance(n + 1);
            }
            Ext16 => {
                let n = self.len(2)?;
                return self.advance(n + 1);
            }
            Ext32 => {
                let n = self.len(4)?;
                return self.advance(n + 1);
            }
            FixArray(n) => (usize::from(n), false),
            Array16 => (self.len(2)?, false),
            Array32 => (self.len(4)?, false),
            FixMap(n) => (usize::from(n), true),
            Map16 => (self.len(2)?, true),
            Map32 => (self.len(4)?, true),
        };

        let depth = depth.checked_sub(1).ok_or(Malformed)?;
        for _ in 0..items {
            if pairs {
                self.skip(depth)?;
            }
            self.skip(depth)?;
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct Success<'a, T: ?Sized> {
    jsonrpc: &'static str,
    id: &'a Id,
    result: &'a T,
}

#[derive(Serialize)]
struct Failure<'a, E: ?Sized> {
    jsonrpc: &'static str,
    id: &'a Id,
    error: ErrorObject<'a, E>,
}

#[derive(Serialize)]
struct ErrorObject<'a, E: ?Sized> {
    code: i32,
    message: &'a str,
    data: &'a ErrorData<'a, E>,
}

/// `error.data`: класс ошибки провода, `retry_after` и типизированная
/// ошибка операции, когда она есть.
#[derive(Serialize)]
pub struct ErrorData<'a, E: ?Sized> {
    pub code: ErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<HumanDuration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<&'a E>,
}

impl ErrorData<'_, ()> {
    pub const fn plain(code: ErrorCode) -> Self {
        Self {
            code,
            retry_after: None,
            error: None,
        }
    }
}

fn dump<T: Serialize + ?Sized>(format: Format, what: &T) -> Vec<u8> {
    format.dump(what).unwrap_or_else(|e| {
        log::error!(error = e.as_str(); "http response encoding failed");
        let failure = Failure {
            jsonrpc: "2.0",
            id: &Id::Null,
            error: ErrorObject {
                code: code::INTERNAL_ERROR,
                message: "response encoding failed",
                data: &ErrorData::<()>::plain(ErrorCode::Internal),
            },
        };
        format.dump(&failure).unwrap_or_default()
    })
}

pub fn success<T: Serialize + ?Sized>(format: Format, id: &Id, result: &T) -> Vec<u8> {
    dump(
        format,
        &Success {
            jsonrpc: "2.0",
            id,
            result,
        },
    )
}

pub fn failure<E: Serialize + ?Sized>(
    format: Format,
    id: &Id,
    code: i32,
    message: &str,
    data: &ErrorData<'_, E>,
) -> Vec<u8> {
    dump(
        format,
        &Failure {
            jsonrpc: "2.0",
            id,
            error: ErrorObject {
                code,
                message,
                data,
            },
        },
    )
}

/// Ошибка транспорта или моста: без типизированной ошибки операции.
pub fn transport(format: Format, id: &Id, error: &RpcError) -> Vec<u8> {
    let data = ErrorData::<()> {
        code: error.code,
        retry_after: error.retry_after,
        error: None,
    };
    failure(format, id, code_of(error.code), &error.message, &data)
}

/// Ошибка операции (`Generic<E>`) или `Aux`: как `codec::typed` на проводе.
pub fn typed<E: Serialize + HasCode + fmt::Display>(format: Format, id: &Id, error: &E) -> Vec<u8> {
    let code = error.code();
    let data = ErrorData {
        code,
        retry_after: error.retry_after(),
        error: Some(error),
    };
    failure(format, id, code_of(code), &error.to_string(), &data)
}

/// Склеивает готовые ответы в batch без перекодирования.
pub fn batch(format: Format, responses: &[Vec<u8>]) -> Vec<u8> {
    let payload: usize = responses.iter().map(Vec::len).sum();
    let mut body = Vec::with_capacity(payload + responses.len() + 5);

    match format {
        Format::Json => {
            body.push(b'[');
            for (i, response) in responses.iter().enumerate() {
                if i != 0 {
                    body.push(b',');
                }
                body.extend_from_slice(response);
            }
            body.push(b']');
        }
        Format::Msgpack => {
            let len = responses.len();
            if let Ok(n) = u8::try_from(len)
                && n < 16
            {
                body.push(Marker::FixArray(n).to_u8());
            } else if let Ok(n) = u16::try_from(len) {
                body.push(Marker::Array16.to_u8());
                body.extend_from_slice(&n.to_be_bytes());
            } else {
                body.push(Marker::Array32.to_u8());
                body.extend_from_slice(&u32::try_from(len).unwrap_or(u32::MAX).to_be_bytes());
            }
            for response in responses {
                body.extend_from_slice(response);
            }
        }
    }

    body
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use eva_protocol::{errors::Aux, types::Bytes};

    use super::*;

    fn msgpack(value: &Value) -> Vec<u8> {
        rmp_serde::to_vec_named(value).unwrap()
    }

    #[test]
    fn json_single_and_batch() {
        let body =
            br#"{"jsonrpc":"2.0","id":7,"method":"surfaces.get","params":{"surface":"local"}}"#;
        let Message::Single(request) = parse(Format::Json, body, 8).unwrap() else {
            panic!("single");
        };
        assert_eq!(request.id, Some(Id::Uint(7)));
        assert_eq!(request.method, "surfaces.get");
        assert!(matches!(request.params, Params::Json(_)));

        let body = br#"[{"jsonrpc":"2.0","method":"a"},{"jsonrpc":"1.0","id":1,"method":"b"},3]"#;
        let Message::Batch(batch) = parse(Format::Json, body, 8).unwrap() else {
            panic!("batch");
        };
        assert_eq!(batch.len(), 3);
        assert!(batch[0].as_ref().is_ok_and(|r| r.id.is_none()));
        assert!(
            batch[1]
                .as_ref()
                .is_err_and(|r| r.code == code::INVALID_REQUEST)
        );
        assert!(
            batch[2]
                .as_ref()
                .is_err_and(|r| r.code == code::INVALID_REQUEST)
        );

        assert_eq!(
            parse(Format::Json, b"[]", 8).err().map(|r| r.code),
            Some(code::INVALID_REQUEST)
        );
        assert_eq!(
            parse(Format::Json, b"[{},{},{}]", 2).err().map(|r| r.code),
            Some(code::INVALID_REQUEST)
        );
        assert_eq!(
            parse(Format::Json, b"{not json", 8).err().map(|r| r.code),
            Some(code::PARSE_ERROR)
        );
    }

    #[test]
    fn msgpack_single_and_batch_mirror_json() {
        let body = msgpack(
            &json!({"jsonrpc": "2.0", "id": "x", "method": "surfaces.get", "params": {"surface": "local"}}),
        );
        let Message::Single(request) = parse(Format::Msgpack, &body, 8).unwrap() else {
            panic!("single");
        };
        assert_eq!(request.id, Some(Id::Str("x".into())));
        assert_eq!(request.method, "surfaces.get");
        let Params::Msgpack(raw) = request.params else {
            panic!("params stay msgpack");
        };
        let decoded: Value = rmp_serde::from_slice(&raw).unwrap();
        assert_eq!(decoded, json!({"surface": "local"}));

        let body = msgpack(&json!([
            {"jsonrpc": "2.0", "method": "a", "params": null},
            {"jsonrpc": "2.0", "id": null, "method": "b", "params": [1]},
            "junk",
        ]));
        let Message::Batch(batch) = parse(Format::Msgpack, &body, 8).unwrap() else {
            panic!("batch");
        };
        assert!(
            batch[0]
                .as_ref()
                .is_ok_and(|r| r.id.is_none() && matches!(r.params, Params::Absent))
        );
        assert!(
            batch[1]
                .as_ref()
                .is_ok_and(|r| r.id == Some(Id::Null) && matches!(r.params, Params::NotAnObject))
        );
        assert!(batch[2].is_err());

        let mut trailing = msgpack(&json!({"jsonrpc": "2.0", "method": "a"}));
        trailing.push(0xc0);
        assert_eq!(
            parse(Format::Msgpack, &trailing, 8).err().map(|r| r.code),
            Some(code::PARSE_ERROR)
        );
        assert_eq!(
            parse(
                Format::Msgpack,
                &[0x81, 0xa6, b'm', b'e', b't', b'h', b'o', b'd'],
                8
            )
            .err()
            .map(|r| r.code),
            Some(code::PARSE_ERROR)
        );
    }

    /// Байты в msgpack-params — `bin`, их пропуск считает длину верно.
    #[test]
    fn msgpack_params_may_carry_bin() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Args {
            data: Bytes,
            tag: u8,
        }
        let args = Args {
            data: Bytes(vec![0xc0; 300]),
            tag: 9,
        };
        let mut body = vec![0x83];
        for (key, value) in [
            ("jsonrpc", rmp_serde::to_vec_named("2.0").unwrap()),
            ("params", rmp_serde::to_vec_named(&args).unwrap()),
            ("method", rmp_serde::to_vec_named("kv.set").unwrap()),
        ] {
            body.extend(rmp_serde::to_vec_named(key).unwrap());
            body.extend(value);
        }
        let Message::Single(request) = parse(Format::Msgpack, &body, 8).unwrap() else {
            panic!("single");
        };
        assert_eq!(request.method, "kv.set");
        assert_eq!(request.params.decode::<Args>().unwrap(), args);
    }

    #[test]
    fn params_decode_defaults_when_absent() {
        #[derive(Deserialize, Default, PartialEq, Debug)]
        struct Args {
            #[serde(default)]
            limit: u8,
        }
        assert_eq!(Params::Absent.decode::<Args>().unwrap(), Args::default());
        assert!(Params::NotAnObject.decode::<Args>().is_err());
    }

    #[test]
    fn error_data_carries_code_and_typed_error() {
        let aux = Aux::forbidden("owner only");
        let body = typed(Format::Json, &Id::Uint(1), &aux);
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            value,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": {
                    "code": code::FORBIDDEN,
                    "message": "forbidden: owner only",
                    "data": {"code": "forbidden", "error": {"forbidden": "owner only"}},
                }
            })
        );

        let body = transport(Format::Msgpack, &Id::Null, &RpcError::unknown_method("x"));
        let value: Value = rmp_serde::from_slice(&body).unwrap();
        assert_eq!(
            value,
            json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {
                    "code": code::METHOD_NOT_FOUND,
                    "message": "unknown method x",
                    "data": {"code": "unknown_method"},
                }
            })
        );
    }

    #[test]
    fn batch_joins_without_reencoding() {
        let parts = [
            success(Format::Json, &Id::Uint(1), &json!({"a": 1})),
            success(Format::Json, &Id::Uint(2), &json!({"b": 2})),
        ];
        let value: Value = serde_json::from_slice(&batch(Format::Json, &parts)).unwrap();
        assert_eq!(value.as_array().map(Vec::len), Some(2));

        let parts: Vec<Vec<u8>> = (0..20)
            .map(|n| success(Format::Msgpack, &Id::Uint(n), &json!({"n": n})))
            .collect();
        let value: Value = rmp_serde::from_slice(&batch(Format::Msgpack, &parts)).unwrap();
        assert_eq!(value.as_array().map(Vec::len), Some(20));
        assert_eq!(value[19]["result"]["n"], json!(19));
    }
}
