//! Секция `module` конфига модуля (`06 §11`): сервис слушает ядро
//! (`ServiceConfig`), клиент звонит ему сам (`ClientConfig`). Свои секции
//! модуль регистрирует рядом в том же `App`.

use std::time::Duration;

use eva::{cli::App, data, time::HumanDuration, url::Url, utils::SecretString};

use eva_rpc::{ClientTls, ConnectError, ServiceListen};

/// Сервис: ядро набирает его по записи `modules.services.<id>`.
#[data]
pub struct ServiceConfig {
    /// `{uds: /run/eva-<id>/module.sock, mode?, group?}` или `{tcp: "127.0.0.1:7001"}`;
    /// при socket activation (`LISTEN_FDS`) — вид унаследованного сокета.
    pub listen: ServiceListen,
    /// Тот же секрет, что `key` записи реестра у ядра: `{plaintext: …}` или `{file: …}`.
    pub key: SecretString,
    /// Сколько `Listening::kernel` ждёт первого соединения ядра.
    #[serde(default = "default_wait")]
    pub kernel_wait: HumanDuration,
    /// Тайминги и лимиты соединения — те же ключи, что у ядра.
    #[serde(flatten, default)]
    pub transport: eva_rpc::Config,
}

impl ServiceConfig {
    /// Секция `module` без дефолта: адрес слушателя и ключ обязательны.
    pub fn register(app: App) -> App {
        app.require::<Self>("module")
    }
}

/// Клиент хозяина (телефон, TUI): сам звонит ядру с токеном.
#[data]
pub struct ClientConfig {
    /// `uds:///run/eva-kernel/modules.sock`, `ws://…/v1/modules`, `wss://…`.
    #[schemars(with = "String")]
    pub kernel: Url,
    /// `{plaintext: …}` или `{file: …}`; файл — например `LoadCredential` systemd.
    pub token: SecretString,
    /// Сколько ждать ядро на старте, прежде чем сдаться.
    #[serde(default = "default_wait")]
    pub connect_wait: HumanDuration,
    #[serde(default)]
    pub tls: ClientTls,
    /// Тайминги и лимиты соединения — те же ключи, что у ядра.
    #[serde(flatten, default)]
    pub transport: eva_rpc::Config,
}

impl ClientConfig {
    /// Секция `module` без дефолта: адрес ядра и токен обязательны.
    pub fn register(app: App) -> App {
        app.require::<Self>("module")
    }
}

/// `connect_wait` клиента и `kernel_wait` сервиса.
pub(crate) const DEFAULT_WAIT: Duration = Duration::from_secs(180);

fn default_wait() -> HumanDuration {
    HumanDuration(DEFAULT_WAIT)
}

/// Секрет читается один раз; ошибка называет ключ конфига.
pub(crate) fn read_token(token: &SecretString, key: &str) -> Result<String, ConnectError> {
    token
        .read()
        // файл секрета обычно кончается переводом строки, в токен он не входит
        .map(|token| token.trim_end().to_owned())
        .map_err(|e| ConnectError::Config(format!("{key}: {e:#}")))
}
