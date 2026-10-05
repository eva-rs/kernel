//! Секция `modules` (`06 §11`): слушатели и тайминги соединения — типы
//! `eva-rpc` (плоско в той же секции), остальное — ядра.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use eva::{bytesize::ByteSize, data, time::HumanDuration, utils::SecretString};

use eva_protocol::types::{ModuleId, module::ServiceAddress};
use eva_rpc::config::{Listen, UdsListen};

#[data]
#[serde(default)]
pub struct ModulesConfig {
    /// Слушатели клиентов (`uds`, `ws`, `wss`, `http`, `https`); относительный
    /// путь сокета — от `system.state_dir`.
    pub listen: Vec<Listen>,
    /// `handshake_timeout`, `ping_*`, `request_timeout`, `stream_*`, `max_inflight`,
    /// `max_streams`, `max_frame`, `max_hello`, `blob_chunk`, `idempotency_window`,
    /// `max_batch`.
    #[serde(flatten)]
    pub transport: eva_rpc::Config,
    /// Ключ — id клиента; токен конфига сильнее выданного (`module_tokens`).
    pub tokens: BTreeMap<ModuleId, TokenEntry>,
    /// Ключ — id сервиса, которого набирает ядро; запись сильнее `modules.add`.
    pub services: BTreeMap<ModuleId, ServiceEntry>,
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
            })],
            transport: eva_rpc::Config::default(),
            tokens: BTreeMap::new(),
            services: BTreeMap::new(),
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

/// Токен клиента из конфига.
#[data]
pub struct TokenEntry {
    /// `{plaintext: …}` или `{file: …}`; читается один раз на старте.
    pub token: SecretString,
}

impl TokenEntry {
    pub const fn new(token: SecretString) -> Self {
        Self { token }
    }
}

/// Сервис из конфига: куда звонить и общий с модулем секрет PSK (`06 §2.1`).
#[data]
pub struct ServiceEntry {
    pub address: ServiceAddress,
    /// `{plaintext: …}` или `{file: …}`; читается один раз на старте.
    pub key: SecretString,
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
