//! Запуск модуля как приложения eva: конфиг, инициализация и Supervisor.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::Arc,
};

use eva::{
    cli::{App, NoArgs},
    component_configs::{ComponentConfigs, IsComponentConfig},
    data,
    supervisor::Supervisor,
    time::HumanDuration,
    utils::SecretString,
};
use eva_rpc::{ServiceListen, config::UdsListen};

use crate::{Module, ServiceConfig, tool::BoxFuture};

pub(crate) struct Initialization<S> {
    register: Box<dyn FnOnce(App) -> App + Send>,
    initialize: Box<dyn FnOnce(ComponentConfigs) -> BoxFuture<eyre::Result<S>> + Send>,
}

impl Module {
    pub fn init<C, S, F, Fut>(self, section: impl Into<String>, initialize: F) -> Module<S>
    where
        C: IsComponentConfig,
        S: Send + Sync + 'static,
        F: FnOnce(C) -> Fut + Send + 'static,
        Fut: Future<Output = eyre::Result<S>> + Send + 'static,
    {
        let section = section.into();
        assert!(
            section != "module",
            "module is reserved for SDK configuration"
        );
        let mut module = self.retype();
        module.initialization = Some(Initialization {
            register: Box::new(move |app| app.require::<C>(&section)),
            initialize: Box::new(move |configs| {
                let config = (*configs.get::<C>()).clone();
                Box::pin(initialize(config))
            }),
        });
        module
    }
}

impl<S: Send + Sync + 'static> Module<S> {
    /// Дефолты зависят от окончательного имени, поэтому вычисляются после всех сеттеров.
    pub fn run(mut self) -> eyre::Result<()> {
        let prefix = env_prefix(self.module_name())?;
        let mut app = App::default()
            .env_prefix(&prefix)
            .optional::<ServiceDefaults>("module");
        let initialization = if let Some(init) = self.initialization.take() {
            app = (init.register)(app);
            Some(init.initialize)
        } else {
            None
        };
        app.run(move |_: NoArgs, configs: ComponentConfigs| async move {
            let config = configs
                .get::<ServiceDefaults>()
                .resolve(self.module_name())?;
            if let Some(initialize) = initialization {
                self.state = Some(Arc::new(initialize(configs.clone()).await?));
            }
            let listening = self.listen(&config).await?;
            Supervisor::new(configs)
                .add::<ServiceDefaults>(move |_, mut shutdown| {
                    let listening = listening.clone();
                    async move {
                        listening
                            .run(async move {
                                let _ = shutdown.recv().await;
                            })
                            .await
                    }
                })
                .wait_for_completion()
                .await
        })
    }
}

/// Необязательная секция приложения; явный ServiceConfig остаётся пригоден для встраивания.
#[data]
#[derive(Default)]
#[serde(default)]
pub struct ServiceDefaults {
    pub listen: Option<ServiceListen>,
    pub key: Option<SecretString>,
    pub soul_wait: Option<HumanDuration>,
    #[serde(flatten)]
    pub transport: eva_rpc::Config,
}

impl ServiceDefaults {
    pub fn resolve(&self, name: &str) -> eyre::Result<ServiceConfig> {
        let directory = std::env::var_os("RUNTIME_DIRECTORY");
        self.resolve_in(name, directory.as_deref())
    }

    fn resolve_in(&self, name: &str, directory: Option<&OsStr>) -> eyre::Result<ServiceConfig> {
        env_prefix(name)?;
        let listen = match &self.listen {
            Some(listen) => listen.clone(),
            None => ServiceListen::Uds(UdsListen {
                path: socket_path(name, directory)?,
            }),
        };
        Ok(ServiceConfig {
            listen,
            key: self
                .key
                .clone()
                .unwrap_or_else(|| SecretString::credential("soul-key")),
            soul_wait: self
                .soul_wait
                .unwrap_or(HumanDuration(crate::config::DEFAULT_WAIT)),
            transport: self.transport.clone(),
        })
    }
}

fn env_prefix(name: &str) -> eyre::Result<String> {
    if name.is_empty()
        || name.len() > 64
        || !name.as_bytes()[0].is_ascii_alphabetic()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        eyre::bail!(
            "module name must start with a letter and contain at most 64 letters, digits, '-' or '_'"
        );
    }
    Ok(format!("{}_", name.replace('-', "_").to_ascii_uppercase()))
}

fn socket_path(name: &str, directory: Option<&OsStr>) -> eyre::Result<PathBuf> {
    let directory = match directory {
        Some(directory) => {
            let path = Path::new(directory);
            if !path.is_absolute() || directory.as_encoded_bytes().contains(&b':') {
                eyre::bail!(
                    "RUNTIME_DIRECTORY must name one absolute directory; set module.listen explicitly otherwise"
                );
            }
            path.to_owned()
        }
        None => PathBuf::from(format!("/run/eva-{name}")),
    };
    Ok(directory.join("module.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_the_final_name_and_environment() {
        let defaults = ServiceDefaults::default();
        let cfg = defaults.resolve_in("owm", None).unwrap();
        let ServiceListen::Uds(uds) = cfg.listen else {
            panic!("uds");
        };
        assert_eq!(uds.path, PathBuf::from("/run/eva-owm/module.sock"));
        assert_eq!(cfg.key, SecretString::credential("soul-key"));
        assert_eq!(env_prefix("weather-bridge").unwrap(), "WEATHER_BRIDGE_");
        assert_eq!(
            socket_path("owm", Some(OsStr::new("/run/custom"))).unwrap(),
            PathBuf::from("/run/custom/module.sock")
        );
        assert!(socket_path("owm", Some(OsStr::new("/one:/two"))).is_err());
        for name in ["", "../module", "a/b", "1bad"] {
            assert!(env_prefix(name).is_err());
        }
    }

    #[test]
    fn explicit_values_override_inferred_values() {
        let defaults: ServiceDefaults = serde_json::from_value(serde_json::json!({
            "listen": {"tcp":"127.0.0.1:7001"},
            "key":{"plaintext":"explicit"}, "soul_wait":"5s", "ping_interval":"3s"
        }))
        .unwrap();
        let cfg = defaults
            .resolve_in("owm", Some(OsStr::new("invalid")))
            .unwrap();
        assert!(matches!(cfg.listen, ServiceListen::Tcp(_)));
        assert_eq!(cfg.key.read().unwrap(), "explicit");
        assert_eq!(cfg.soul_wait.as_secs(), 5);
        assert_eq!(cfg.transport.ping_interval.as_secs(), 3);
    }
}
