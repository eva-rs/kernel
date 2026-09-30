//! Реестр сервисов (`06 §2.1`): записи `modules.services` и строки
//! `module_services` без затенённых конфигом. Петли дозвона держит компонент
//! `modules` (`dial.rs`); операции `modules.add/remove` правят их отсюда.

use std::{
    collections::BTreeMap,
    sync::{Mutex, MutexGuard},
};

use eva::time::Timestamp;
use eyre::bail;
use tokio::sync::mpsc;

use eva_protocol::types::{
    ModuleId,
    module::{Dial, ServiceAddress},
};
use eva_rpc::Psk;

use crate::{
    config::modules::{ServiceEntry, TokenEntry},
    handling::modules::{RESERVED, check_name},
    subsystems::modules::{live::Source, tokens::ConfigTokens},
};

/// Ключ короче — NNpsk0 с ним подбирается офлайн по перехваченному handshake.
pub const WEAK_KEY: usize = 32;

/// Секрет так, как его видит `Psk::derive`: без хвостового перевода строки файла.
pub fn trimmed(secret: &str) -> &str {
    secret.trim_end_matches(['\n', '\r'])
}

/// Запись реестра: куда звонить и PSK; сам секрет в памяти не держится.
#[derive(Debug, Clone)]
pub struct Service {
    pub address: ServiceAddress,
    pub psk: Psk,
    pub source: Source,
}

/// Команда петлям дозвона компонента `modules`.
#[derive(Debug)]
pub enum Control {
    Start(ModuleId, Service),
    Stop(ModuleId),
}

/// Запись реестра для `modules.list` и `stats.health`.
#[derive(Debug, Clone)]
pub struct Listed {
    pub dial: Dial,
    /// `added_at` строки или старт ядра у записи конфига.
    pub since: Timestamp,
}

#[derive(Debug, Default)]
struct State {
    /// Последний провал дозвона по id, до следующего `welcome`.
    errors: BTreeMap<ModuleId, String>,
    /// `None` — компонент `modules` не поднят: строки БД подхватит его старт.
    dialer: Option<mpsc::UnboundedSender<Control>>,
}

#[derive(Debug, Default)]
pub struct Services {
    config: BTreeMap<ModuleId, Service>,
    state: Mutex<State>,
    /// `modules.add/remove` и старт петель по одному: порядок строк БД —
    /// порядок команд петлям.
    ops: tokio::sync::Mutex<()>,
}

impl Services {
    /// Зарезервированное или невалидное имя, id и в `modules.tokens`,
    /// нечитаемый или пустой ключ — ошибка старта с путём ключа.
    pub fn load(
        services: &BTreeMap<ModuleId, ServiceEntry>,
        tokens: &BTreeMap<ModuleId, TokenEntry>,
    ) -> eyre::Result<Self> {
        let mut config = BTreeMap::new();
        for (module, entry) in services {
            if RESERVED.contains(&module.as_str()) {
                bail!("modules.services.{module}: {module} is a reserved module id");
            }
            if let Err(e) = check_name(module) {
                bail!("modules.services.{module}: {e}");
            }
            if tokens.contains_key(module) {
                bail!(
                    "modules.services.{module} and modules.tokens.{module}: a module id is either a service or a client"
                );
            }
            let read = entry
                .key
                .read()
                .map_err(|e| eyre::eyre!("modules.services.{module}.key: {e:#}"))?;
            let secret = trimmed(&read);
            if secret.is_empty() {
                bail!("modules.services.{module}.key is empty");
            }
            let len = secret.chars().count();
            if len < WEAK_KEY {
                log::warn!(module:% = module, len, min = WEAK_KEY; "modules.services key is short: its handshake can be brute-forced offline");
            }
            config.insert(
                module.clone(),
                Service {
                    address: entry.address.clone(),
                    psk: Psk::derive(secret),
                    source: Source::ConfigService,
                },
            );
        }
        Ok(Self {
            config,
            ..Self::default()
        })
    }

    /// Записи `modules.services`.
    pub fn config(&self) -> &BTreeMap<ModuleId, Service> {
        &self.config
    }

    /// Конфиг называет сервис: строка БД с тем же id инертна.
    pub fn configures(&self, module: &ModuleId) -> bool {
        self.config.contains_key(module)
    }

    /// Строка БД с этим id затенена конфигом (`06 §2`).
    pub fn shadowed(&self, module: &ModuleId, tokens: &ConfigTokens) -> bool {
        self.configures(module) || tokens.configures(module)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Держать на время записи в `module_services` и команды петлям.
    pub async fn ops(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.ops.lock().await
    }

    /// Компонент `modules` поднялся: команды идут его петлям.
    pub(crate) fn attach(&self, dialer: mpsc::UnboundedSender<Control>) {
        self.lock().dialer = Some(dialer);
    }

    pub(crate) fn detach(&self) {
        self.lock().dialer = None;
    }

    /// Запускает петлю дозвона записи; без компонента — её запустит его старт.
    pub fn start(&self, module: ModuleId, service: Service) {
        let mut state = self.lock();
        // Провал попытки, заставшей прежний `remove`, к новой записи не относится.
        state.errors.remove(&module);
        if let Some(dialer) = &state.dialer {
            _ = dialer.send(Control::Start(module, service));
        }
    }

    /// Останавливает петлю записи и забывает её провал.
    pub fn stop(&self, module: &ModuleId) {
        let mut state = self.lock();
        state.errors.remove(module);
        if let Some(dialer) = &state.dialer {
            _ = dialer.send(Control::Stop(module.clone()));
        }
    }

    pub fn failed(&self, module: &ModuleId, error: String) {
        self.lock().errors.insert(module.clone(), error);
    }

    pub fn welcomed(&self, module: &ModuleId) {
        self.lock().errors.remove(module);
    }

    /// Реестр для списков: конфиг и незатенённые строки `module_services`
    /// (`rows` — id, адрес, `added_at`).
    pub fn listed(
        &self,
        rows: Vec<(ModuleId, ServiceAddress, Timestamp)>,
        tokens: &ConfigTokens,
        started_at: Timestamp,
    ) -> BTreeMap<ModuleId, Listed> {
        let errors = self.lock().errors.clone();
        let entry = |module: &ModuleId, address: ServiceAddress, config: bool, since| Listed {
            dial: Dial {
                address,
                config,
                error: errors.get(module).cloned(),
            },
            since,
        };

        let mut listed: BTreeMap<ModuleId, Listed> = self
            .config
            .iter()
            .map(|(module, service)| {
                let listed = entry(module, service.address.clone(), true, started_at);
                (module.clone(), listed)
            })
            .collect();
        for (module, address, added_at) in rows {
            if !self.shadowed(&module, tokens) {
                let row = entry(&module, address, false, added_at);
                listed.insert(module, row);
            }
        }
        listed
    }
}
