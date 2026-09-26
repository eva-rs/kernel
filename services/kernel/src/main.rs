use std::sync::Mutex;

use eva::{
    cli::{App, NoArgs},
    component_configs::ComponentConfigs,
    supervisor::{Handle, SlaveRx, Supervisor},
};

use eva_kernel::{
    Config, Kernel, Real,
    config::{DbConfig, ModulesConfig},
    db::{Db as _, DbError},
    subsystems::modules,
};

fn main() -> eyre::Result<()> {
    Config::register(App::default().env_prefix("EVA_")).run(entrypoint)
}

async fn entrypoint(_: NoArgs, cfgs: ComponentConfigs) -> eyre::Result<()> {
    // HTTP-клиенты провайдеров и wss строят rustls без явного провайдера.
    eva_rpc::install_crypto_provider();

    let cfg = Config::from_configs(&cfgs);
    log::info!(version = eva_kernel::version(), state_dir:% = cfg.system.state_dir.display(); "kernel starting");
    let kernel = Kernel::<Real>::from_config(cfg).await?;
    // До компонентов: занятый сокет роняет процесс, а не одну подсистему.
    let listening = Mutex::new(Some(modules::listen(&kernel).await?));

    let supervisor = Supervisor::new(cfgs)
        .add::<DbConfig>(move |_, rx| hold_db(kernel.clone(), rx))
        .add::<ModulesConfig>(move |_, rx| {
            let listening = listening.lock().unwrap_or_else(|e| e.into_inner()).take();
            async move {
                match listening {
                    Some(listening) => modules::run(listening, rx).await,
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
async fn hold_db(kernel: Kernel<Real>, mut rx: SlaveRx) -> eyre::Result<()> {
    rx.recv().await;
    kernel
        .shared()
        .db()
        .exclusive(|conn| {
            conn.execute_batch("PRAGMA optimize; PRAGMA wal_checkpoint(PASSIVE);")
                .map_err(DbError::from)
        })
        .await?;
    log::info!("database closed");
    Ok(())
}
