//! Общее состояние ядра (`11 §5`): одно на процесс, сессии клонируют `Arc`.

use std::{collections::BTreeMap, sync::Arc};

use eva::{perfect_derive, time::Timestamp};

use crate::{
    bus::Bus,
    components::Components,
    config::Config,
    db::stats::DayZone,
    hands::Endpoints,
    registry::{Models, Snapshot},
    serving::Serving,
    subsystems::modules::live::Live,
    world::{Demiurge, World},
};

pub type DemiurgeOf<C> = <C as Components>::Demiurge;
pub type WorldOf<C> = World<<DemiurgeOf<C> as Demiurge>::World>;
/// Провайдеры по имени из `llm.providers`.
pub type ProvidersOf<C> = BTreeMap<String, Arc<<C as Components>::Provider>>;

/// Ёмкость шины: отставший подписчик теряет старые события, не тормозит ядро.
const BUS_CAPACITY: usize = 1024;

struct Inner<C: Components> {
    cfg: Config,
    db: C::Db,
    demiurge: C::Demiurge,
    started_at: Timestamp,
    models: Snapshot<Models>,
    providers: Snapshot<ProvidersOf<C>>,
    /// Пересборка реестра под замком: две сессии-провайдера не теряют друг друга.
    registry: std::sync::Mutex<()>,
    hands: Endpoints<C::Hands>,
    bus: Bus,
    serving: Serving,
    live: Live,
}

#[perfect_derive(Clone)]
pub struct Shared<C: Components>(Arc<Inner<C>>);

impl<C: Components> Shared<C> {
    pub(crate) fn new(
        cfg: Config,
        db: C::Db,
        demiurge: C::Demiurge,
        providers: ProvidersOf<C>,
        hands: Endpoints<C::Hands>,
    ) -> Self {
        let started_at = demiurge.now();
        let models = Models::build(&providers, &cfg.llm);
        Self(Arc::new(Inner {
            cfg,
            db,
            demiurge,
            started_at,
            models: Snapshot::new(models),
            providers: Snapshot::new(providers),
            registry: std::sync::Mutex::default(),
            hands,
            bus: Bus::new(BUS_CAPACITY),
            serving: Serving::default(),
            live: Live::default(),
        }))
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
        DayZone::from_tz(self.0.cfg.system.tz.as_deref())
    }

    /// Старт ядра: отсчёт `uptime` в `stats.health`.
    pub fn started_at(&self) -> Timestamp {
        self.0.started_at
    }

    /// Снимок реестра моделей на время запроса.
    pub fn models(&self) -> Arc<Models> {
        self.0.models.get()
    }

    pub fn providers(&self) -> Arc<ProvidersOf<C>> {
        self.0.providers.get()
    }

    pub fn provider(&self, name: &str) -> Option<Arc<C::Provider>> {
        self.0.providers.get().get(name).cloned()
    }

    /// Провайдер пришёл (`Some`) или ушёл (`None`): реестр моделей
    /// пересобирается из `models()` всех провайдеров.
    pub fn set_provider(&self, name: &str, provider: Option<C::Provider>) -> Arc<Models> {
        let _guard = self.0.registry.lock().unwrap_or_else(|e| e.into_inner());
        let mut providers = ProvidersOf::<C>::clone(&self.0.providers.get());
        match provider {
            Some(provider) => providers.insert(name.to_owned(), Arc::new(provider)),
            None => providers.remove(name),
        };
        let models = Models::build(&providers, &self.0.cfg.llm);
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

    pub fn serving(&self) -> &Serving {
        &self.0.serving
    }

    /// Живые сессии модулей по имени.
    pub fn live(&self) -> &Live {
        &self.0.live
    }
}
