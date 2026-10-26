//! `Soul<C>: SessionMaker` (`11 §5`): старт — БД, миграции, сид, реестры;
//! слушателей нет, их поднимает подсистема `modules`.

use std::collections::BTreeMap;

use eva::perfect_derive;
use eyre::WrapErr as _;

use eva_core::{Caller, SessionMaker, service};
use eva_protocol::errors::Aux;

use crate::{
    components::{Components, Real},
    config::Config,
    db::{Db as _, DbResult, Queries as _, Sqlite, migrate, seed, stats::DayZone, surfaces},
    hands::{self, Endpoints},
    providers::SoulProvider,
    session::Session,
    shared::Shared,
    state::State,
    subsystems::modules::Registrations,
    world::Demiurge as _,
};

/// Компоненты, из которых собирается soul: в бою — из конфига, в тесте — руками.
pub struct Parts<C: Components> {
    pub demiurge: C::Demiurge,
    pub db: C::Db,
    /// Встроенные провайдеры по имени из `llm.providers`.
    pub providers: BTreeMap<String, C::Provider>,
    /// Машины рук по имени из `hands.endpoints`.
    pub hands: BTreeMap<String, C::Hands>,
}

impl Parts<Real> {
    pub async fn from_config(cfg: &Config) -> eyre::Result<Self> {
        let db = Sqlite::open(
            &cfg.db_path(),
            *cfg.db.busy_timeout,
            usize::from(cfg.db.readers.into_inner()),
        )
        .await
        .wrap_err("failed to open the database")?;
        let providers = SoulProvider::from_config(&cfg.llm).await?;

        Ok(Self {
            demiurge: crate::world::God,
            db,
            providers,
            hands: hands::from_config(&cfg.hands)?,
        })
    }
}

impl Soul<Real> {
    /// Боевой soul: компоненты из конфига.
    pub async fn from_config(cfg: Config) -> eyre::Result<Self> {
        let parts = Parts::from_config(&cfg).await?;
        Self::start(cfg, parts).await
    }
}

#[perfect_derive(Clone)]
pub struct Soul<C: Components> {
    shared: Shared<C>,
}

impl<C: Components> Soul<C> {
    pub async fn start(cfg: Config, parts: Parts<C>) -> eyre::Result<Self> {
        let Parts {
            demiurge,
            db,
            providers,
            hands,
        } = parts;
        let zone = DayZone::from_tz(cfg.system.tz.as_deref())?;
        let registrations = Registrations::load(&cfg.modules)?;
        check_masters(&cfg)?;
        cfg.check()?;
        let now = demiurge.now();

        let applied = db
            .exclusive(move |conn| migrate::run(conn, migrate::MIGRATIONS, now))
            .await
            .wrap_err("database migrations refused to start")?;

        let persons = cfg.chats.persons.clone();
        let seeded = db
            .write(move |tx| seed::run(tx, &persons, now))
            .await
            .wrap_err("failed to seed the database")?;
        if let Some(master) = &seeded.person {
            log::info!(person:% = master; "master identity seeded");
        }
        for pending in &seeded.pending {
            log::warn!(surface:% = pending.surface; "master identity waits for its surface");
        }
        for foreign in &seeded.foreign {
            log::warn!(surface:% = foreign.surface, module:? = foreign.module; "master identity skipped: its surface belongs to another module");
        }
        if !seeded.unmerged.is_empty() {
            log::warn!(persons:? = seeded.unmerged; "master identities belong to several persons: merge them");
        }

        // Сессии прошлого процесса мертвы: модули вернутся своим hello.
        let forget = *cfg.modules.offline_forget;
        let (offline, forgotten) = db
            .write(move |tx| {
                let offline = tx.modules().all_offline(now)?;
                DbResult::Ok((offline, tx.modules().forget(now, forget)?))
            })
            .await
            .wrap_err("failed to reset module states")?;
        if offline + forgotten > 0 {
            log::info!(offline, forgotten; "module states reset after restart");
        }

        let hands = Endpoints::connect(hands).await;
        if let Some(default) = &cfg.hands.default_endpoint
            && hands.get(default).is_none()
        {
            log::warn!(endpoint = default.as_str(); "hands.default_endpoint is not configured");
        }

        match crate::handling::blobs::sweep(&cfg.blob_dir()).await {
            Ok(0) => {}
            Ok(removed) => log::info!(removed; "stale blob uploads removed"),
            Err(e) => log::warn!(error:% = e; "failed to sweep stale blob uploads"),
        }

        let shadowed = db
            .reader()
            .await?
            .run(|c| {
                let mut ids = c.modules().token_modules()?;
                ids.extend(c.modules().services()?.into_iter().map(|row| row.module));
                DbResult::Ok(ids)
            })
            .await
            .wrap_err("failed to read module registrations")?;
        for module in shadowed.iter().filter(|m| registrations.shadowed(m)) {
            log::warn!(module:% = module; "the config names this module: its database registration is ignored");
        }

        let shared = Shared::new(cfg, zone, db, demiurge, providers, hands, registrations).await?;
        crate::subsystems::turns::recovery::recover(&shared).await?;
        let mut tools = crate::subsystems::turns::soul_tools::decls(shared.cfg());
        tools.extend(shared.ports().toolset.decls(shared.cfg()));
        crate::subsystems::tools::registry::register(
            &shared,
            &eva_protocol::types::ModuleId::from("soul"),
            &crate::subsystems::turns::soul_tools::groups(),
            &tools,
        )
        .await?;
        shared.settle_providers();
        crate::subsystems::work::agents::recover(&shared).await?;
        let default_model = &shared.cfg().llm.default_model;
        if shared.models().get(default_model).is_none() {
            log::warn!(model:% = default_model; "default model is not in the registry yet");
        }

        log::info!(migrations = applied, models = shared.models().iter().count(); "soul started");
        Ok(Self { shared })
    }

    pub fn shared(&self) -> &Shared<C> {
        &self.shared
    }

    pub(crate) fn from_shared(shared: Shared<C>) -> Self {
        Self { shared }
    }
}

/// Адрес хозяина на surface, который может создать любой сервис, отдаёт ему
/// master-туры: без привязки к модулю — предупреждение.
fn check_masters(cfg: &Config) -> eyre::Result<()> {
    for pair in &cfg.chats.persons.master {
        let builtin = [surfaces::LOCAL, surfaces::SYSTEM].contains(&pair.surface.as_str());
        match (&pair.module, builtin) {
            (Some(module), true) => eyre::bail!(
                "chats.persons.master: builtin surface {} has no module, yet it is bound to {module}",
                pair.surface
            ),
            (None, false) => {
                log::warn!(surface:% = pair.surface; "chats.persons.master surface is claimable by any service: bind it to a module");
            }
            _ => {}
        }
    }
    Ok(())
}

impl<C: Components> SessionMaker for Soul<C> {
    type Session = Session<C>;

    async fn make_session(&self, caller: Caller) -> Result<service::Session<Session<C>>, Aux> {
        // Пир закрытой сессии мог прочесть запрос до закрытия: отзыв сильнее.
        if let Some(reason) = caller.module().and_then(|m| self.shared.live().closed(m)) {
            return Err(Aux::Unavailable(format!(
                "module session is closed: {}",
                reason.message
            )));
        }
        let shared = self.shared.clone();
        let world = shared.make_world();
        let state = State::new(shared, world, caller.clone());
        Ok(service::Session::new(Session::new(state), caller))
    }
}
