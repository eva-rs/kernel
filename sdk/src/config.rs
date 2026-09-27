//! Секция `module` конфига модуля (`06 §10`): куда подключаться и с каким
//! токеном. Свои секции модуль регистрирует рядом в том же `App`.

use std::time::Duration;

use eva::{cli::App, data, time::HumanDuration, url::Url, utils::SecretString};

use eva_rpc::{ClientTls, ConnectError};

#[data]
pub struct ModuleConfig {
    /// `uds:///run/eva-kernel/modules.sock`, `ws://…/v1/modules`, `wss://…`.
    #[schemars(with = "String")]
    pub kernel: Url,
    /// `{plaintext: …}` или `{file: …}`; файл — например `LoadCredential` systemd.
    pub token: SecretString,
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

/// Секрет читается один раз; ошибка называет ключ конфига.
pub(crate) fn read_token(token: &SecretString, key: &str) -> Result<String, ConnectError> {
    token
        .read()
        // файл секрета обычно кончается переводом строки, в токен он не входит
        .map(|token| token.trim_end().to_owned())
        .map_err(|e| ConnectError::Config(format!("{key}: {e:#}")))
}
