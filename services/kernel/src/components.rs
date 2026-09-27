//! Компоненты ядра (`11 §5`): что подменяет тест.

use eva_core::Provider;
use eva_rpc::ModuleProvider;

use crate::{db::Db, hands::Machine, providers::Settle, world::Demiurge};

pub trait Components: Send + Sync + Sized + 'static {
    /// Часы и rng; в тестах время двигает тест.
    type Demiurge: Demiurge;
    /// SQLite (`10 §7`); в тестах — обёртка с инъекцией отказов.
    type Db: Db;
    /// `KernelProvider`; модуль запасной двери встаёт в реестр через `From`,
    /// встроенный дочитывает каталог моделей через `Settle`.
    type Provider: Provider + From<ModuleProvider> + Settle + 'static;
    /// `eva_hands::Hands`; в тестах — счётчик с инъекцией `Unavailable`.
    type Hands: Machine;
}

/// Боевой набор.
pub struct Real;

impl Components for Real {
    type Demiurge = crate::world::God;
    type Db = crate::db::Sqlite;
    type Provider = crate::providers::KernelProvider;
    type Hands = eva_hands::Hands;
}
