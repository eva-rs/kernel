//! Руки (`07 §3`): трейт над методами `eva_hands::Hands`, которые зовут
//! тулы ядра, и endpoint'ы из `hands.endpoints` с лимитами из greeting.
//! Трейт нужен тестам: счётчик вызовов и инъекция `Error::Unavailable`.

use std::{collections::BTreeMap, future::Future, sync::Arc, time::Duration};

use eva_hands::{Entry, Error, Greeting, Hands, Limits, Output};
use eyre::bail;

use crate::config::{HandsConfig, hands::EndpointConfig};

/// Машина, на которой исполняются `fs_*`/`shell_*`.
pub trait Machine: Send + Sync + 'static {
    fn greeting(&self) -> impl Future<Output = Result<Greeting, Error>> + Send;

    fn stat(
        &self,
        cwd: Option<&str>,
        path: &str,
    ) -> impl Future<Output = Result<Entry, Error>> + Send;

    /// Записи каталога и флаг «список обрезан лимитом».
    fn list(
        &self,
        cwd: Option<&str>,
        path: &str,
    ) -> impl Future<Output = Result<(Vec<Entry>, bool), Error>> + Send;

    /// Размер файла и окно `[offset, offset + len)`.
    fn read(
        &self,
        cwd: Option<&str>,
        path: &str,
        offset: u64,
        len: Option<u64>,
    ) -> impl Future<Output = Result<(u64, Vec<u8>), Error>> + Send;

    fn write(
        &self,
        cwd: Option<&str>,
        path: &str,
        data: &[u8],
        create_dirs: bool,
        mode: Option<u32>,
    ) -> impl Future<Output = Result<u64, Error>> + Send;

    /// Всего совпадений, смещения с `from` не больше `limit`, флаг обрезки.
    fn find(
        &self,
        cwd: Option<&str>,
        path: &str,
        pattern: &[u8],
        from: u64,
        limit: u64,
    ) -> impl Future<Output = Result<(u64, Vec<u64>, bool), Error>> + Send;

    fn mkdir(
        &self,
        cwd: Option<&str>,
        path: &str,
        parents: bool,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    fn remove(
        &self,
        cwd: Option<&str>,
        path: &str,
        recursive: bool,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    fn rename(
        &self,
        cwd: Option<&str>,
        from: &str,
        to: &str,
    ) -> impl Future<Output = Result<(), Error>> + Send;

    fn exec(
        &self,
        cwd: Option<&str>,
        command: &str,
        timeout_ms: u64,
        stdin: Vec<u8>,
    ) -> impl Future<Output = Result<Output, Error>> + Send;
}

impl Machine for Hands {
    async fn greeting(&self) -> Result<Greeting, Error> {
        Hands::greeting(self).await
    }

    async fn stat(&self, cwd: Option<&str>, path: &str) -> Result<Entry, Error> {
        Hands::stat(self, cwd, path).await
    }

    async fn list(&self, cwd: Option<&str>, path: &str) -> Result<(Vec<Entry>, bool), Error> {
        Hands::list(self, cwd, path).await
    }

    async fn read(
        &self,
        cwd: Option<&str>,
        path: &str,
        offset: u64,
        len: Option<u64>,
    ) -> Result<(u64, Vec<u8>), Error> {
        Hands::read(self, cwd, path, offset, len).await
    }

    async fn write(
        &self,
        cwd: Option<&str>,
        path: &str,
        data: &[u8],
        create_dirs: bool,
        mode: Option<u32>,
    ) -> Result<u64, Error> {
        Hands::write(self, cwd, path, data, create_dirs, mode).await
    }

    async fn find(
        &self,
        cwd: Option<&str>,
        path: &str,
        pattern: &[u8],
        from: u64,
        limit: u64,
    ) -> Result<(u64, Vec<u64>, bool), Error> {
        Hands::find(self, cwd, path, pattern, from, limit).await
    }

    async fn mkdir(&self, cwd: Option<&str>, path: &str, parents: bool) -> Result<(), Error> {
        Hands::mkdir(self, cwd, path, parents).await
    }

    async fn remove(&self, cwd: Option<&str>, path: &str, recursive: bool) -> Result<(), Error> {
        Hands::remove(self, cwd, path, recursive).await
    }

    async fn rename(&self, cwd: Option<&str>, from: &str, to: &str) -> Result<(), Error> {
        Hands::rename(self, cwd, from, to).await
    }

    async fn exec(
        &self,
        cwd: Option<&str>,
        command: &str,
        timeout_ms: u64,
        stdin: Vec<u8>,
    ) -> Result<Output, Error> {
        Hands::exec(self, cwd, command, timeout_ms, stdin).await
    }
}

/// Машины из `hands.endpoints`: `Hands::local` или демон по Noise.
pub fn from_config(cfg: &HandsConfig) -> eyre::Result<BTreeMap<String, Hands>> {
    let mut out = BTreeMap::new();
    for (name, endpoint) in &cfg.endpoints {
        let hands = match endpoint {
            EndpointConfig::Local {
                local: true,
                limits,
            } => Hands::local(Limits::from(limits.clone())),
            EndpointConfig::Local { local: false, .. } => {
                bail!(
                    "hands.endpoints.{name}: `local` must be true or be replaced by {{address, key}}"
                )
            }
            EndpointConfig::Remote { address, key } => Hands::remote(address.as_str(), key),
        };
        out.insert(name.clone(), hands);
    }
    Ok(out)
}

/// Сколько ждать greeting на старте: недоступный демон старт не держит.
const GREETING_WAIT: Duration = Duration::from_secs(3);

/// Endpoint рук: машина и её лимиты из последнего greeting.
pub struct Endpoint<M> {
    pub name: String,
    pub machine: M,
    limits: tokio::sync::RwLock<Option<Limits>>,
}

impl<M: Machine> Endpoint<M> {
    /// Лимиты машины: кэш greeting, иначе новый greeting (после реконнекта).
    pub async fn limits(&self) -> Result<Limits, Error> {
        if let Some(limits) = self.limits.read().await.as_ref() {
            return Ok(limits.clone());
        }
        let greeting = self.machine.greeting().await?;
        *self.limits.write().await = Some(greeting.limits.clone());
        Ok(greeting.limits)
    }

    /// Машина отвалилась: следующий вызов перечитает лимиты.
    pub async fn forget_limits(&self) {
        *self.limits.write().await = None;
    }
}

/// Все endpoint'ы по имени.
pub struct Endpoints<M>(BTreeMap<String, Arc<Endpoint<M>>>);

impl<M: Machine> Endpoints<M> {
    /// Greeting каждой машины параллельно; недоступная остаётся без лимитов.
    pub async fn connect(machines: BTreeMap<String, M>) -> Self {
        let greet = machines.into_iter().map(|(name, machine)| async move {
            let limits = match tokio::time::timeout(GREETING_WAIT, machine.greeting()).await {
                Ok(Ok(greeting)) => Some(greeting.limits),
                Ok(Err(error)) => {
                    log::warn!(endpoint = name.as_str(), error:%; "hands endpoint greeting failed");
                    None
                }
                Err(_) => {
                    log::warn!(endpoint = name.as_str(); "hands endpoint greeting timed out");
                    None
                }
            };
            let endpoint = Endpoint {
                name: name.clone(),
                machine,
                limits: tokio::sync::RwLock::new(limits),
            };
            (name, Arc::new(endpoint))
        });

        Self(futures::future::join_all(greet).await.into_iter().collect())
    }

    pub fn get(&self, name: &str) -> Option<&Arc<Endpoint<M>>> {
        self.0.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }
}
