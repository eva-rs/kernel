//! Секция `modules` (`06 §11`): слушатели и тайминги соединения — типы
//! `eva-rpc` (плоско в той же секции), остальное — ядра.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use eva::{bytesize::ByteSize, data, time::HumanDuration, utils::SecretString};

use eva_protocol::types::{ModuleId, module::TokenKind};
use eva_rpc::config::{FileMode, Listen, UdsListen};

#[data]
#[serde(default)]
pub struct ModulesConfig {
    /// Относительный путь сокета — от `system.state_dir`.
    pub listen: Vec<Listen>,
    /// `handshake_timeout`, `ping_*`, `request_timeout`, `stream_*`, `max_inflight`,
    /// `max_streams`, `max_frame`, `max_hello`, `blob_chunk`, `idempotency_window`,
    /// `proto_min_major`.
    #[serde(flatten)]
    pub transport: eva_rpc::Config,
    /// Ключ — id модуля; токен конфига сильнее выданного тулом (`module_tokens`).
    pub tokens: BTreeMap<ModuleId, TokenEntry>,
    pub call_reconnect_grace: HumanDuration,
    pub offline_forget: HumanDuration,
    pub hooks: HooksConfig,
    #[schemars(with = "String")]
    pub fs_quota: ByteSize,
    /// Файлов и каталогов под `fs/` модуля: пустые записи байтовая квота не видит.
    pub fs_max_entries: u32,
    pub sqlite_max_rows: u32,
    pub sqlite_busy_timeout: HumanDuration,
    /// Потолок `state.sqlite` модуля; столько же — temp-базе и временным файлам вызова.
    #[schemars(with = "String")]
    pub sqlite_quota: ByteSize,
    /// Ключи и значения `module_kv` одного модуля вместе.
    #[schemars(with = "String")]
    pub kv_quota: ByteSize,
    #[schemars(with = "String")]
    pub kv_value_max: ByteSize,
}

impl Default for ModulesConfig {
    fn default() -> Self {
        Self {
            listen: vec![Listen::Uds(UdsListen {
                path: PathBuf::from("modules.sock"),
                mode: FileMode::default(),
                group: Some(String::from("eva")),
            })],
            transport: eva_rpc::Config::default(),
            tokens: BTreeMap::new(),
            call_reconnect_grace: secs(30),
            offline_forget: secs(7 * 24 * 60 * 60),
            hooks: HooksConfig::default(),
            fs_quota: ByteSize::gib(1),
            fs_max_entries: 100_000,
            sqlite_max_rows: 10_000,
            sqlite_busy_timeout: secs(5),
            sqlite_quota: ByteSize::gib(1),
            kv_quota: ByteSize::mib(64),
            kv_value_max: ByteSize::mib(1),
        }
    }
}

impl ModulesConfig {
    /// Данных в одном ответе: `max_frame` за вычетом конверта кадра.
    pub fn max_payload(&self) -> u64 {
        self.transport.max_payload()
    }
}

const fn secs(n: u64) -> HumanDuration {
    HumanDuration(Duration::from_secs(n))
}

/// Токен модуля из конфига: секрет и единственный бит прав.
#[data]
pub struct TokenEntry {
    /// `{plaintext: …}` или `{file: …}`; читается один раз на старте.
    pub token: SecretString,
    pub kind: TokenKind,
}

#[data]
#[serde(default)]
pub struct HooksConfig {
    /// Потолок `hooks.timeout` модуля.
    pub timeout: HumanDuration,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self { timeout: secs(2) }
    }
}
