//! Секция `modules` (`06 §11`): слушатели и тайминги соединения — типы
//! `eva-rpc` (плоско в той же секции), остальное — ядра.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use eva::{bytesize::ByteSize, data, time::HumanDuration};

use eva_protocol::types::{ModuleId, module::TokenKind};
use eva_rpc::config::{FileMode, Listen, UdsListen};

use crate::config::Secret;

#[data]
#[serde(default)]
pub struct ModulesConfig {
    /// Относительный путь сокета — от `system.state_dir`.
    pub listen: Vec<Listen>,
    /// `handshake_timeout`, `ping_*`, `request_timeout`, `stream_*`, `max_frame`,
    /// `blob_chunk`, `idempotency_window`, `proto_min_major`.
    #[serde(flatten)]
    pub transport: eva_rpc::Config,
    /// Токены из конфига сильнее выданных тулом (`module_tokens`).
    pub tokens: BTreeMap<ModuleId, TokenEntry>,
    pub call_reconnect_grace: HumanDuration,
    pub offline_forget: HumanDuration,
    pub hooks: HooksConfig,
    #[schemars(with = "String")]
    pub fs_quota: ByteSize,
    pub sqlite_max_rows: u32,
    pub sqlite_busy_timeout: HumanDuration,
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
            sqlite_max_rows: 10_000,
            sqlite_busy_timeout: secs(5),
        }
    }
}

const fn secs(n: u64) -> HumanDuration {
    HumanDuration(Duration::from_secs(n))
}

/// Токен модуля из конфига: секрет и единственный бит прав.
#[data]
pub struct TokenEntry {
    pub token: Secret,
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
