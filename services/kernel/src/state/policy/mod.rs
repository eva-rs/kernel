//! `Policy` (`11 §4`): единственное место авторизации. Проверки без
//! объекта (`owner`, `kernel`, `module`) — первой строкой хендлера;
//! объектные — сразу после загрузки объекта, до первой мутации. Чужой
//! объект — `Aux::Forbidden`, не `NotFound`.
//!
//! Файл домена сам добавляет свой `policy.<ns>()`, поэтому правила домена
//! не трогают этот модуль. Домены, чьи операции только для `C` (§3),
//! зовут `owner()` напрямую.

use eva_core::Caller;
use eva_protocol::{
    errors::{Aux, AuxResult, ext::BoolExt as _},
    types::ModuleId,
};

mod agents;
mod blobs;
mod capabilities;
mod chats;
mod cron;
mod events;
mod folders;
mod fs;
mod hands_grants;
mod inference;
mod kv;
mod memory;
mod models;
mod modules;
mod notes;
mod persons;
mod sleep;
mod sqlite;
mod stats;
mod surfaces;
mod tools;
mod turns;

pub use self::{
    capabilities::CapabilitiesPolicy,
    chats::ChatsPolicy,
    events::EventsPolicy,
    memory::MemoryPolicy,
    modules::ModulesPolicy,
    stats::StatsPolicy,
    surfaces::{OnSurface, SurfacesPolicy},
};

/// Caller внутренних сессий ядра (крон, сон, GC): конструируется только здесь.
pub const fn kernel_caller() -> Caller {
    Caller::Kernel
}

#[derive(Debug, Clone, Copy)]
pub struct Policy<'a> {
    caller: &'a Caller,
}

impl<'a> Policy<'a> {
    pub const fn new(caller: &'a Caller) -> Self {
        Self { caller }
    }

    pub const fn caller(self) -> &'a Caller {
        self.caller
    }

    /// Ядро или клиент хозяина: операции с `C` без `S` в §3.
    pub fn owner(self) -> AuxResult<()> {
        self.caller.is_owner().true_or(Aux::forbidden("owner only"))
    }

    /// Только само ядро (`chats.notify`).
    pub fn kernel(self) -> AuxResult<()> {
        self.caller
            .is_kernel()
            .true_or(Aux::forbidden("kernel only"))
    }

    /// Только клиент хозяина (`modules.add/remove`): ни тул модели, ни ядро
    /// не наводят дозвон на произвольный адрес.
    pub fn client(self) -> AuxResult<&'a ModuleId> {
        match self.caller {
            Caller::Client { module } => Ok(module),
            Caller::Kernel | Caller::Service { .. } => Err(Aux::forbidden("owner client only")),
        }
    }

    /// Имя модуля сессии: `fs`/`sqlite`/`kv`, `surfaces.create`.
    pub fn module(self) -> AuxResult<&'a ModuleId> {
        self.caller
            .module()
            .ok_or_else(|| Aux::forbidden("a module session is required"))
    }

    /// `trusted`, `model`, `effort`, `thinking`, `compact_at` на surface/folder/chat.
    pub fn set_trust(self) -> AuxResult<()> {
        self.caller
            .is_owner()
            .true_or(Aux::forbidden("trust and model settings are owner only"))
    }
}
