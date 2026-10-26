//! Capabilities модуля (`06 §3`). Отсутствующая секция — capability не
//! объявлена; пустой список — ровно ноль элементов (I8).
//!
//! Surface — не capability, а сущность soul: модуль создаёт её `surfaces.create`.

use eva::{data, time::HumanDuration};

use crate::{
    types::{
        model::{ModelDecl, ProviderDecl},
        tool::{GroupDecl, ToolDecl},
    },
    wire::{events::Topic, hooks::HookPoint},
};

#[data]
#[derive(Default)]
#[serde(default)]
pub struct Capabilities {
    /// Запасной вход провайдера; принимается, только если модуль назван в `llm.providers`.
    pub provider: Option<ProviderCap>,
    pub tools: Option<ToolsCap>,
    pub events: Option<EventsCap>,
    pub hooks: Option<HooksCap>,
    /// Модуль рисует кнопки pending-запросов в чатах surface, которые обслуживает.
    pub ask: bool,
}

impl Capabilities {
    /// Ключи, которые понимает эта версия протокола; прочие — `warnings` (`06 §8`).
    pub const KEYS: [&str; 5] = ["provider", "tools", "events", "hooks", "ask"];

    /// Имена объявленных секций — строка `modules.list`.
    pub fn names(&self) -> Vec<String> {
        [
            ("provider", self.provider.is_some()),
            ("tools", self.tools.is_some()),
            ("events", self.events.is_some()),
            ("hooks", self.hooks.is_some()),
            ("ask", self.ask),
        ]
        .into_iter()
        .filter(|&(_, declared)| declared)
        .map(|(name, _)| name.to_owned())
        .collect()
    }
}

/// Те же типы, что отдают `Provider::decl()`/`models()`.
#[data]
pub struct ProviderCap {
    pub decl: ProviderDecl,
    pub models: Vec<ModelDecl>,
}

#[data]
#[derive(Default)]
#[serde(default)]
pub struct ToolsCap {
    pub groups: Vec<GroupDecl>,
    pub tools: Vec<ToolDecl>,
}

#[data]
#[derive(Default)]
#[serde(default)]
pub struct EventsCap {
    pub subscriptions: Vec<Topic>,
}

#[data]
#[derive(Default)]
#[serde(default)]
pub struct HooksCap {
    /// Зовутся только объявленные точки.
    pub points: Vec<HookPoint>,
    /// Потолок — `modules.hooks.timeout`.
    pub timeout: Option<HumanDuration>,
    pub fail_mode: FailMode,
}

/// Что делать, если хук не ответил вовремя, упал или модуль offline.
#[data(copy, ord, display(name))]
#[derive(Default)]
#[serde(remote = "FailMode")]
pub enum FailMode {
    /// `cancel` с маркером `hook <module>.<point> unavailable`.
    Closed,
    /// `continue`; сюда же — режим новее soul.
    #[default]
    #[serde(other)]
    Open,
}

tolerant!(FailMode);
