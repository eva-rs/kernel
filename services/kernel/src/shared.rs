//! Общее состояние ядра (`11 §5`): одно на процесс, сессии клонируют `Arc`.

use std::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};

use eva::{perfect_derive, time::Timestamp};
use tokio::task::JoinSet;

use eva_core::{GenSink, Provider};
use eva_protocol::types::{
    ModuleId,
    generate::{EmbedRequest, EmbedResponse, GenerateRequest, ProviderError},
    model::{ModelDecl, ProviderDecl},
};

use crate::{
    bus::Bus,
    components::Components,
    config::Config,
    db::{
        Db as _, Queries as _,
        stats::{DayZone, Metric},
    },
    hands::Endpoints,
    providers::Settle as _,
    registry::{self, Models, Snapshot},
    subsystems::modules::{live::Live, tokens::ConfigTokens},
    world::{Demiurge, World},
};

pub type DemiurgeOf<C> = <C as Components>::Demiurge;
pub type WorldOf<C> = World<<DemiurgeOf<C> as Demiurge>::World>;
/// Провайдеры по имени из `llm.providers`.
pub type ProvidersOf<C> = BTreeMap<String, Arc<Registered<C>>>;

/// Ёмкость шины: отставший подписчик теряет старые события, не тормозит ядро.
const BUS_CAPACITY: usize = 1024;

struct Inner<C: Components> {
    cfg: Config,
    db: C::Db,
    demiurge: C::Demiurge,
    started_at: Timestamp,
    zone: DayZone,
    models: Snapshot<Models>,
    providers: Snapshot<ProvidersOf<C>>,
    /// Правка реестра под замком: две сессии-провайдера не теряют друг друга.
    registry: std::sync::Mutex<()>,
    hands: Endpoints<C::Hands>,
    bus: Bus,
    tokens: ConfigTokens,
    live: Live,
    /// Дочитывание каталогов встроенных провайдеров; снимаются вместе с ядром.
    settling: std::sync::Mutex<JoinSet<()>>,
}

#[perfect_derive(Clone)]
pub struct Shared<C: Components>(Arc<Inner<C>>);

impl<C: Components> Shared<C> {
    /// `zone` — уже проверенная `system.tz`: неподдержанная роняет старт раньше.
    pub(crate) fn new(
        cfg: Config,
        zone: DayZone,
        db: C::Db,
        demiurge: C::Demiurge,
        providers: BTreeMap<String, C::Provider>,
        hands: Endpoints<C::Hands>,
        tokens: ConfigTokens,
    ) -> Self {
        let started_at = demiurge.now();
        let providers: ProvidersOf<C> = providers
            .into_iter()
            .map(|(name, provider)| {
                let registered = Registered {
                    provider,
                    module: None,
                };
                (name, Arc::new(registered))
            })
            .collect();
        let models = Models::build(&providers, &cfg.llm);
        Self(Arc::new(Inner {
            cfg,
            db,
            demiurge,
            started_at,
            zone,
            models: Snapshot::new(models),
            providers: Snapshot::new(providers),
            registry: std::sync::Mutex::default(),
            hands,
            bus: Bus::new(BUS_CAPACITY),
            tokens,
            live: Live::default(),
            settling: std::sync::Mutex::default(),
        }))
    }

    /// Встроенный провайдер без каталога на старте дочитывает его фоном, и
    /// его часть реестра пересобирается: иначе модели до рестарта не появятся.
    pub(crate) fn settle_providers(&self) {
        let mut settling = self.0.settling.lock().unwrap_or_else(|e| e.into_inner());
        for (name, registered) in self.providers().iter() {
            let (name, registered) = (name.clone(), registered.clone());
            // Слабая ссылка: задача внутри `Inner` не держит его живым.
            let inner = Arc::downgrade(&self.0);
            settling.spawn(async move {
                if registered.provider.settle().await
                    && let Some(inner) = inner.upgrade()
                {
                    Shared(inner).reload_provider(&name);
                }
            });
        }
    }

    /// Модели провайдера `name` заново из его `models()`.
    fn reload_provider(&self, name: &str) {
        let _guard = self.0.registry.lock().unwrap_or_else(|e| e.into_inner());
        let Some(registered) = self.0.providers.get().get(name).cloned() else {
            return;
        };
        let llm = &self.0.cfg.llm;
        let mut models = Models::clone(&self.0.models.get());
        models.remove_provider(name);
        models.add(
            name,
            registered.decl(),
            registered.models(),
            registry::allow_list(llm, name),
            llm,
        );
        let count = models.iter().filter(|m| m.provider == name).count();
        let default = models.get(&llm.default_model).is_some();
        self.0.models.replace(models);
        log::info!(provider = name, models = count, default_model = default; "provider models reloaded");
    }

    pub fn cfg(&self) -> &Config {
        &self.0.cfg
    }

    pub fn db(&self) -> &C::Db {
        &self.0.db
    }

    pub fn demiurge(&self) -> &C::Demiurge {
        &self.0.demiurge
    }

    pub fn make_world(&self) -> WorldOf<C> {
        self.0.demiurge.make_world()
    }

    /// Время без мира (`Demiurge::now`).
    pub fn now(&self) -> Timestamp {
        self.0.demiurge.now()
    }

    /// Зона суток счётчиков и разрезов `stats` (`system.tz`).
    pub fn day_zone(&self) -> DayZone {
        self.0.zone.clone()
    }

    /// Старт ядра: отсчёт `uptime` в `stats.health`.
    pub fn started_at(&self) -> Timestamp {
        self.0.started_at
    }

    /// +1 к счётчику здоровья отдельной записью: сбой счётчика не валит
    /// то, что он считал, и уходит только в лог.
    pub async fn bump(&self, metric: Metric, key: &str) {
        let (now, zone, owned) = (self.now(), self.day_zone(), key.to_owned());
        let written = self
            .db()
            .write(move |tx| tx.stats().bump(metric, &owned, now, &zone))
            .await;
        if let Err(e) = written {
            log::error!(metric = metric.as_str(), key, error:% = e; "failed to bump a health counter");
        }
    }

    /// Снимок реестра моделей на время запроса.
    pub fn models(&self) -> Arc<Models> {
        self.0.models.get()
    }

    pub fn providers(&self) -> Arc<ProvidersOf<C>> {
        self.0.providers.get()
    }

    pub fn provider(&self, name: &str) -> Option<Arc<Registered<C>>> {
        self.0.providers.get().get(name).cloned()
    }

    /// Провайдер модуля запасной двери пришёл (`Some`) или ушёл (`None`).
    /// Реестр правится поверх текущего: имя модели, которое уже кто-то
    /// обслуживает, модуль не перехватит ни порядком имён, ни реконнектом.
    pub fn set_module_provider(
        &self,
        module: &ModuleId,
        provider: Option<C::Provider>,
    ) -> Arc<Models> {
        let _guard = self.0.registry.lock().unwrap_or_else(|e| e.into_inner());
        let (name, llm) = (module.as_str(), &self.0.cfg.llm);
        let mut providers = ProvidersOf::<C>::clone(&self.0.providers.get());
        let mut models = Models::clone(&self.0.models.get());
        models.remove_provider(name);
        match provider {
            Some(provider) => {
                let registered = Arc::new(Registered {
                    provider,
                    module: Some((module.clone(), Arc::downgrade(&self.0))),
                });
                models.add(
                    name,
                    registered.decl(),
                    registered.models(),
                    registry::allow_list(llm, name),
                    llm,
                );
                providers.insert(name.to_owned(), registered);
            }
            None => {
                providers.remove(name);
            }
        }
        self.0.providers.replace(providers);
        self.0.models.replace(models);
        self.0.models.get()
    }

    pub fn hands(&self) -> &Endpoints<C::Hands> {
        &self.0.hands
    }

    pub fn bus(&self) -> &Bus {
        &self.0.bus
    }

    /// Токены `modules.tokens`, прочитанные на старте.
    pub fn tokens(&self) -> &ConfigTokens {
        &self.0.tokens
    }

    /// Живые сессии модулей по имени.
    pub fn live(&self) -> &Live {
        &self.0.live
    }
}

/// Провайдер реестра; модуль запасной двери считает ошибки своих RPC
/// в `modules.rpc_errors{module}` (`09 §5`), кто бы его ни звал.
pub struct Registered<C: Components> {
    provider: C::Provider,
    /// Слабая ссылка: провайдер живёт внутри того же `Shared`.
    module: Option<(ModuleId, Weak<Inner<C>>)>,
}

impl<C: Components> Registered<C> {
    async fn counted<T: Send>(&self, res: Result<T, ProviderError>) -> Result<T, ProviderError> {
        if res.is_err()
            && let Some((module, inner)) = &self.module
            && let Some(inner) = inner.upgrade()
        {
            Shared(inner)
                .bump(Metric::ModulesRpcErrors, module.as_str())
                .await;
        }
        res
    }
}

impl<C: Components> Provider for Registered<C> {
    fn decl(&self) -> ProviderDecl {
        self.provider.decl()
    }

    fn models(&self) -> Vec<ModelDecl> {
        self.provider.models()
    }

    async fn generate(&self, req: GenerateRequest, sink: GenSink) -> Result<(), ProviderError> {
        let res = self.provider.generate(req, sink).await;
        self.counted(res).await
    }

    async fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse, ProviderError> {
        let res = self.provider.embed(req).await;
        self.counted(res).await
    }

    async fn count_tokens(&self, req: GenerateRequest) -> Result<u32, ProviderError> {
        let res = self.provider.count_tokens(req).await;
        self.counted(res).await
    }
}
