use std::sync::Mutex;

use eva::{
    cli::{App, NoArgs},
    component_configs::ComponentConfigs,
    supervisor::{Handle, SlaveRx, Supervisor},
};
use tokio::sync::watch;

use eva_kernel::{
    Config, Kernel, Real,
    config::{DbConfig, ModulesConfig},
    db::{self, Db as _},
    subsystems::modules,
};

fn main() -> eyre::Result<()> {
    Config::register(App::default().env_prefix("EVA_")).run(entrypoint)
}

async fn entrypoint(_: NoArgs, cfgs: ComponentConfigs) -> eyre::Result<()> {
    // HTTP-клиенты провайдеров строят rustls без явного провайдера.
    eva_rpc::install_crypto_provider();

    let cfg = Config::from_configs(&cfgs);
    log::info!(version = eva_kernel::version(), state_dir:% = cfg.system.state_dir.display(); "kernel starting");
    let kernel = Kernel::<Real>::from_config(cfg).await?;
    // Сендер живёт ровно столько, сколько компонент `modules`.
    let (modules_alive, modules_gone) = watch::channel(());
    // До компонентов: занятый сокет роняет процесс, а не одну подсистему.
    let listening = Mutex::new(Some((modules::listen(&kernel).await?, modules_alive)));

    let supervisor = Supervisor::new(cfgs)
        .add::<DbConfig>(move |_, rx| hold_db(kernel.clone(), rx, modules_gone.clone()))
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
    let handles = supervisor.iter().map(|(_, h)| h.clone()).collect();
    tokio::spawn(drain_on_signal(handles));
    supervisor.wait_for_completion().await
}

/// Первый SIGINT/SIGTERM сворачивает компоненты штатно, второй обрывает
/// процесс: `Supervisor` закреплённой версии `eva` сигналов не слушает.
async fn drain_on_signal(handles: Vec<Handle>) {
    use tokio::signal::unix::{SignalKind, signal};

    let mut term = match signal(SignalKind::terminate()) {
        Ok(term) => term,
        Err(error) => {
            log::error!(error:%; "failed to listen for SIGTERM");
            return;
        }
    };
    let mut draining = false;
    loop {
        let signal = tokio::select! {
            res = tokio::signal::ctrl_c() => match res {
                Ok(()) => "SIGINT",
                Err(error) => {
                    log::error!(error:%; "failed to listen for SIGINT");
                    return;
                }
            },
            _ = term.recv() => "SIGTERM",
        };
        if draining {
            log::warn!(signal; "second shutdown signal, exiting now");
            std::process::exit(1);
        }
        log::info!(signal; "shutdown signal received, draining components");
        draining = true;
        for handle in &handles {
            _ = handle.request_shutdown().await;
        }
    }
}

/// Миграции и сид прошли в `Kernel::start`; компонент держит базу до
/// `Shutdown` и сбрасывает WAL в основной файл.
async fn hold_db(
    kernel: Kernel<Real>,
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
    kernel
        .shared()
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
