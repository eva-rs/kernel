//! `.surface()` — сахар над `surfaces.*` после каждого `welcome` (`06 §3`):
//! surface не capability, а сущность ядра, которую модуль создаёт сам.

use eva::data;

use eva_core::prelude::*;
use eva_protocol::{
    errors::Generic,
    requests::{
        Patch,
        surfaces::{create, get, update},
    },
    types::SurfaceId,
    wire::RpcError,
};

use crate::Kernel;

/// Встроенный surface клиентов хозяина.
pub const LOCAL: &str = "local";

/// Лицо модуля. `trusted`/`model`/`effort` модуль не объявляет — их ставит хозяин.
#[data]
pub struct Surface {
    pub id: SurfaceId,
    /// `None` — имя равно `id`.
    pub name: Option<String>,
    /// Prompt-блок канала; модуль владеет его текстом.
    pub rules: Option<String>,
    /// Модуль рисует кнопки pending-запросов.
    pub ask: bool,
    /// Сессия лишь обслуживает встроенный `local`.
    pub builtin: bool,
}

impl Surface {
    pub fn new(id: impl Into<SurfaceId>) -> Self {
        Self {
            id: id.into(),
            name: None,
            rules: None,
            ask: false,
            builtin: false,
        }
    }

    /// Клиент хозяина обслуживает `local`: без имени и правил, их меняет только хозяин.
    pub fn local() -> Self {
        Self {
            builtin: true,
            ..Self::new(LOCAL)
        }
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn rules(mut self, rules: impl Into<String>) -> Self {
        self.rules = Some(rules.into());
        self
    }

    pub fn ask(mut self, ask: bool) -> Self {
        self.ask = ask;
        self
    }

    /// Регистрация в текущей сессии: `builtin` — только `create`; иначе
    /// нет своего — `create`, есть — `update` имени, правил и `ask`.
    pub async fn register(&self, kernel: &mut Kernel) -> Result<(), RpcError> {
        if self.builtin {
            let args = create::Args {
                id: self.id.clone(),
                name: None,
                rules: None,
                ask: self.ask,
                builtin: true,
            };
            kernel
                .surfaces()
                .create()
                .call(args)
                .await
                .map_err(rejected)?;
            return Ok(());
        }

        let name = self
            .name
            .clone()
            .unwrap_or_else(|| self.id.as_str().to_owned());
        let found = kernel
            .surfaces()
            .get()
            .call(get::Args {
                surface: self.id.clone(),
            })
            .await;

        match found {
            Ok(_) => {
                let args = update::Args {
                    surface: self.id.clone(),
                    update: update::Update {
                        name: Patch::Change(name),
                        rules: Patch::Change(self.rules.clone()),
                        ask: Patch::Change(self.ask),
                        ..update::Update::default()
                    },
                };
                kernel
                    .surfaces()
                    .update()
                    .call(args)
                    .await
                    .map_err(rejected)?;
            }
            Err(Generic::Spec(get::Err::NotFound(_))) => {
                let args = create::Args {
                    id: self.id.clone(),
                    name: Some(name),
                    rules: self.rules.clone(),
                    ask: self.ask,
                    builtin: false,
                };
                kernel
                    .surfaces()
                    .create()
                    .call(args)
                    .await
                    .map_err(rejected)?;
            }
            Err(error) => return Err(rejected(error)),
        }
        Ok(())
    }
}

/// Отказ операции типом провода: код и `data` сохраняются для вызывающего.
fn rejected<E>(error: Generic<E>) -> RpcError
where
    E: serde::Serialize + eva_protocol::errors::HasCode + std::fmt::Display,
{
    eva_rpc::codec::typed(&error)
}
