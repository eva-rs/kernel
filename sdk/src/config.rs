//! Секция `module` конфига модуля (`06 §10`): куда подключаться и с каким
//! токеном. Свои секции модуль регистрирует рядом в том же `App`.

use std::{fmt, time::Duration};

use eva::{cli::App, data, time::HumanDuration, url::Url};
use serde::{Serialize, Serializer};

use eva_rpc::ClientTls;

/// Секрет конфига: `Debug` и сериализация его не выдают — конфиг уходит в
/// логи, токен приходит только из `EVA_MODULE__TOKEN`/файла учётных данных.
#[data(not(Debug, Serialize))]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl Serialize for Secret {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("***")
    }
}

#[data]
pub struct ModuleConfig {
    /// `uds:///run/eva-kernel/modules.sock`, `ws://…/v1/modules`, `wss://…`.
    #[schemars(with = "String")]
    pub kernel: Url,
    pub token: Secret,
    /// Сколько ждать ядро на старте, прежде чем сдаться.
    #[serde(default = "default_connect_wait")]
    pub connect_wait: HumanDuration,
    #[serde(default)]
    pub tls: ClientTls,
    /// Тайминги и лимиты соединения — те же ключи, что у ядра.
    #[serde(flatten, default)]
    pub transport: eva_rpc::Config,
}

impl ModuleConfig {
    /// Секция `module` без дефолта: адрес ядра и токен обязательны.
    pub fn register(app: App) -> App {
        app.require::<Self>("module")
    }
}

pub(crate) const DEFAULT_CONNECT_WAIT: Duration = Duration::from_secs(180);

fn default_connect_wait() -> HumanDuration {
    HumanDuration(DEFAULT_CONNECT_WAIT)
}
