//! Транспортная часть секции `modules` (`06 §1`, `§11`): ядро встраивает эти
//! типы в свой конфиг, SDK — в свой.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use eva::{bytesize::ByteSize, data, time::HumanDuration};

use eva_protocol::wire::handshake::ProtoVersion;

/// Тайминги и лимиты соединения; одинаковы у ядра и модуля.
#[data]
#[serde(default)]
pub struct Config {
    /// Всё до `welcome` укладывается в это время: у клиента — от accept, у
    /// сервиса — от connect ядра, вместе с Noise.
    pub handshake_timeout: HumanDuration,
    /// Период `notify ping` и `stream_heartbeat` в долгие потоки.
    pub ping_interval: HumanDuration,
    /// Тишина дольше — соединение закрывается, сессия offline.
    pub ping_timeout: HumanDuration,
    /// Ответ на не-потоковый запрос и первый кадр потока.
    pub request_timeout: HumanDuration,
    /// Поток без кадров, включая heartbeat, дольше — `cancel` и `timeout`.
    pub stream_idle_timeout: HumanDuration,
    /// Буфер получателя на каждый поток; переполнение рвёт только этот поток.
    pub stream_buffer: StreamBuffer,
    /// Одновременно обслуживаемых не-потоковых запросов и загрузок сессии;
    /// сверх — сразу `quota`.
    pub max_inflight: SessionLimit,
    /// Одновременно отдаваемых потоков сессии; сверх — сразу `quota`.
    pub max_streams: SessionLimit,
    /// Кадр больше — соединение закрывается с `invalid_params`.
    #[schemars(with = "String")]
    pub max_frame: ByteSize,
    /// Потолок кадра `hello` клиента, урезается до `max_frame`; больше — отказ
    /// `invalid_params`, на который модуль не переподключается. Сервис уже
    /// доказал PSK, его `hello` держит лишь `max_frame`.
    #[schemars(with = "String")]
    pub max_hello: ByteSize,
    /// Потолок чанка `blobs.put`.
    #[schemars(with = "String")]
    pub blob_chunk: ByteSize,
    /// Сколько помнится ответ мутирующего запроса по `(session_id, request.id)`.
    pub idempotency_window: HumanDuration,
    /// Наименьший принимаемый MAJOR `hello.proto_version`; `None` — текущий.
    pub proto_min_major: Option<u16>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            handshake_timeout: secs(10),
            ping_interval: secs(15),
            ping_timeout: secs(45),
            request_timeout: secs(60),
            stream_idle_timeout: secs(120),
            stream_buffer: StreamBuffer::POS64,
            max_inflight: SessionLimit::POS64,
            max_streams: SessionLimit::POS256,
            max_frame: ByteSize::mib(16),
            max_hello: ByteSize::mib(4),
            blob_chunk: ByteSize::mib(1),
            idempotency_window: secs(600),
            proto_min_major: None,
        }
    }
}

/// Конверт кадра `response`/`stream_item` поверх данных: ключи map, `id`, bin-префиксы.
pub const ENVELOPE: u64 = 256;

impl Config {
    /// Данных в одном ответе или элементе потока: `max_frame` за вычетом конверта.
    pub fn max_payload(&self) -> u64 {
        self.max_frame.as_u64().saturating_sub(ENVELOPE)
    }

    pub fn max_frame(&self) -> usize {
        usize::try_from(self.max_frame.as_u64()).unwrap_or(usize::MAX)
    }

    /// Кадр `hello` целиком: `max_hello`, урезанный до `max_frame`.
    pub fn max_hello(&self) -> usize {
        usize::try_from(self.max_hello.as_u64())
            .unwrap_or(usize::MAX)
            .min(self.max_frame())
    }

    pub fn blob_chunk(&self) -> usize {
        usize::try_from(self.blob_chunk.as_u64())
            .unwrap_or(usize::MAX)
            .max(1)
    }

    pub fn stream_buffer(&self) -> usize {
        usize::from(self.stream_buffer.into_inner())
    }

    pub fn max_inflight(&self) -> usize {
        usize::from(self.max_inflight.into_inner())
    }

    pub fn max_streams(&self) -> usize {
        usize::from(self.max_streams.into_inner())
    }

    /// Допустимые MAJOR: `[proto_min_major, текущий]`.
    pub fn accepts(&self, version: ProtoVersion) -> bool {
        let current = ProtoVersion::CURRENT.major;
        let min = self.proto_min_major.unwrap_or(current);
        (min..=current).contains(&version.major)
    }
}

const fn secs(secs: u64) -> HumanDuration {
    HumanDuration(Duration::from_secs(secs))
}

/// Элементов в буфере одного потока.
#[eva::int(u16, 1..=1024)]
pub enum StreamBuffer {}

impl Default for StreamBuffer {
    fn default() -> Self {
        Self::POS64
    }
}

/// Потолок одновременных запросов одной сессии.
#[eva::int(u16, 1..=4096)]
pub enum SessionLimit {}

/// Слушатель клиентов у ядра: `{uds: path}`, `{ws: addr}` или
/// `{wss: addr, tls: {cert, key}}` — вид задаёт ключ адреса.
#[data]
#[serde(untagged)]
pub enum Listen {
    Uds(UdsListen),
    Ws(WsListen),
    Wss(WssListen),
}

/// Права и группу файла сокета задаёт окружение — umask процесса или
/// socket-юнит менеджера служб, — а не конфиг.
#[data]
pub struct UdsListen {
    #[serde(rename = "uds")]
    pub path: PathBuf,
}

/// HTTP нужен только для Upgrade на пути `/v1/modules`.
#[data]
pub struct WsListen {
    #[serde(rename = "ws")]
    pub addr: SocketAddr,
}

#[data]
pub struct WssListen {
    #[serde(rename = "wss")]
    pub addr: SocketAddr,
    pub tls: TlsFiles,
}

/// Слушатель сервисного модуля, куда звонит ядро: `{uds: path}` или
/// `{tcp: "ip:port"}`; оба под Noise.
#[data]
#[serde(untagged)]
pub enum ServiceListen {
    Uds(UdsListen),
    Tcp(TcpListen),
}

#[data]
pub struct TcpListen {
    #[serde(rename = "tcp")]
    pub addr: SocketAddr,
}

/// PEM-файлы сертификата (цепочка) и ключа.
#[data]
pub struct TlsFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// TLS клиента `wss://`.
#[data]
#[derive(Default)]
#[serde(default)]
pub struct ClientTls {
    /// PEM CA ядра с самоподписанным сертификатом: задан — доверие только
    /// ему, иначе публичные корни webpki.
    pub extra_ca: Option<PathBuf>,
}
