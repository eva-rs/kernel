use std::sync::Mutex;

use eva::{
    cli::{App, NoArgs},
    component_configs::ComponentConfigs,
    supervisor::{SlaveRx, Supervisor},
};
use tokio::sync::watch;

use eva_soul::{
    Config, Real, Soul,
    config::{
        ChatsConfig, CronConfig, DbConfig, GcConfig, MemoryConfig, ModulesConfig, SleepConfig,
    },
    db::{self, Db as _},
    subsystems::{cron, gc, memory, modules, search, sleep, turns::driver::Driver},
};

fn main() -> eyre::Result<()> {
    Config::register(App::default().env_prefix("EVA_")).run(entrypoint)
}

async fn entrypoint(_: NoArgs, cfgs: ComponentConfigs) -> eyre::Result<()> {
    // HTTP-клиенты провайдеров строят rustls без явного провайдера.
    eva_rpc::install_crypto_provider();

    let cfg = Config::from_configs(&cfgs);
    log::info!(version = eva_soul::version(), state_dir:% = cfg.system.state_dir.display(); "soul starting");
    let soul = Soul::<Real>::from_config(cfg).await?;
    // Сендер живёт ровно столько, сколько компонент `modules`.
    let (modules_alive, modules_gone) = watch::channel(());
    // До компонентов: занятый сокет роняет процесс, а не одну подсистему.
    let listening = Mutex::new(Some((modules::listen(&soul).await?, modules_alive)));

    let (memory_soul, search_soul) = (soul.clone(), soul.clone());
    let (cron_soul, sleep_soul, gc_soul) = (soul.clone(), soul.clone(), soul.clone());
    let supervisor = Supervisor::new(cfgs)
        .add::<DbConfig>(move |_, rx| hold_db(soul.clone(), rx, modules_gone.clone()))
        .add::<CronConfig>(move |_, rx| {
            cron::Cron::new(cron_soul.clone(), Driver::new(cron_soul.shared().clone())).run(rx)
        })
        .add::<SleepConfig>(move |_, rx| {
            sleep::Sleep::new(sleep_soul.clone(), Driver::new(sleep_soul.shared().clone())).run(rx)
        })
        .add::<GcConfig>(move |_, rx| {
            gc::Gc::new(gc_soul.clone(), Driver::new(gc_soul.shared().clone())).run(rx)
        })
        .add::<MemoryConfig>(move |_, rx| memory::run(memory_soul.shared().clone(), rx))
        .add::<ChatsConfig>(move |_, rx| search::run(search_soul.shared().clone(), rx))
        .add::<ModulesConfig>(move |_, rx| {
            let listening = listening.lock().unwrap_or_else(|e| e.into_inner()).take();
            async move {
                match listening {
                    Some((listening, alive)) => {
                        let res = modules::run(listening, rx).await;
                        drop(alive);
                        res
                    }
                    None => eyre::bail!("the modules component is started once"),
                }
            }
        });
    supervisor.wait_for_completion().await
}

/// Миграции и сид прошли в `Soul::start`; компонент держит базу до
/// `Shutdown` и сбрасывает WAL в основной файл.
async fn hold_db(
    soul: Soul<Real>,
    mut rx: SlaveRx,
    modules: watch::Receiver<()>,
) -> eyre::Result<()> {
    after_modules(
        async move {
            rx.recv().await;
        },
        modules,
    )
    .await;
    soul.shared().sleep().shutdown().await;
    soul.shared().turns().shutdown().await;
    soul.shared().titles().shutdown().await;
    soul.shared()
        .db()
        .exclusive(|conn| db::sqlite::close(conn))
        .await?;
    log::info!("database closed");
    Ok(())
}

/// `Shutdown` приходит всем компонентам без порядка, а сессии модулей пишут
/// в базу до своего закрытия: финальный checkpoint ждёт и конца `modules`.
async fn after_modules(shutdown: impl Future<Output = ()>, mut modules: watch::Receiver<()>) {
    shutdown.await;
    while modules.changed().await.is_ok() {}
}

#[cfg(test)]
mod tests {
    use futures::poll;
    use tokio::sync::oneshot;

    use super::*;

    fn shutdown() -> (oneshot::Sender<()>, impl Future<Output = ()>) {
        let (tx, rx) = oneshot::channel();
        (tx, async move {
            _ = rx.await;
        })
    }

    #[tokio::test]
    async fn the_db_closes_only_after_the_modules_component_ends() {
        let (stop, shutdown) = shutdown();
        let (alive, gone) = watch::channel(());
        let released = after_modules(shutdown, gone);
        tokio::pin!(released);

        assert!(poll!(&mut released).is_pending());
        _ = stop.send(());
        assert!(
            poll!(&mut released).is_pending(),
            "Shutdown alone: module sessions may still be writing"
        );
        drop(alive);
        assert!(poll!(&mut released).is_ready());
    }

    #[tokio::test]
    async fn modules_gone_early_still_wait_for_shutdown() {
        let (stop, shutdown) = shutdown();
        let (alive, gone) = watch::channel(());
        drop(alive);
        let released = after_modules(shutdown, gone);
        tokio::pin!(released);

        assert!(
            poll!(&mut released).is_pending(),
            "a failed modules component does not close the db"
        );
        _ = stop.send(());
        assert!(poll!(&mut released).is_ready());
    }
}
