//! Секция `tools` (`02 §9`): реестр, гейты, исполнение, окно результата,
//! каталог резерва и warm-набор. Пороги — токены и время (I9).

use std::time::Duration;

use eva::{data, time::HumanDuration};

#[data]
#[serde(default, deny_unknown_fields)]
pub struct ToolsConfig {
    /// Visible при любом warm-наборе: пути тулов или групп; гейты — как у всех.
    pub core: Vec<String>,
    /// Глубина пути, включая имя тула.
    pub max_depth: u8,
    /// Потолок одной спеки при регистрации.
    pub max_spec_tokens: u32,
    /// Порог тесноты warm-набора, % окна.
    pub budget_percent: u8,
    /// Таймаут вызова при `ToolDecl.timeout = None`.
    pub call_timeout: HumanDuration,
    /// Потолок `ToolDecl.timeout`; больше — отказ регистрации.
    pub call_timeout_max: HumanDuration,
    pub parallel: Parallel,
    pub result: ResultWindow,
    pub load: Load,
    pub find: Find,
    pub catalog: Catalog,
    pub warm: Warm,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            core: [
                "tools",
                "notes",
                "memory.recall",
                "memory.remember",
                "chat.ask",
            ]
            .map(String::from)
            .into(),
            max_depth: 4,
            max_spec_tokens: 1500,
            budget_percent: 10,
            call_timeout: secs(5 * 60),
            call_timeout_max: secs(15 * 60),
            parallel: Parallel::default(),
            result: ResultWindow::default(),
            load: Load::default(),
            find: Find::default(),
            catalog: Catalog::default(),
            warm: Warm::default(),
        }
    }
}

#[data]
#[serde(default, deny_unknown_fields)]
pub struct Parallel {
    /// Одновременных `parallel_safe` вызовов.
    pub max: u8,
}

impl Default for Parallel {
    fn default() -> Self {
        Self { max: 4 }
    }
}

/// Окно `windowed` (`02 §1.1`).
#[data]
#[serde(default, deny_unknown_fields)]
pub struct ResultWindow {
    pub windowed_max_tokens: u32,
    /// Доля бюджета под голову, остаток — хвост.
    pub head_share: f32,
}

impl Default for ResultWindow {
    fn default() -> Self {
        Self {
            windowed_max_tokens: 2000,
            head_share: 0.7,
        }
    }
}

#[data]
#[serde(default, deny_unknown_fields)]
pub struct Load {
    /// Путей за вызов `tools_load`.
    pub max_paths: u8,
}

impl Default for Load {
    fn default() -> Self {
        Self { max_paths: 8 }
    }
}

#[data]
#[serde(default, deny_unknown_fields)]
pub struct Find {
    /// Дефолт `limit` у `tools_find`.
    pub top_n: u8,
}

impl Default for Find {
    fn default() -> Self {
        Self { top_n: 5 }
    }
}

/// Каталог резерва (`02 §5.1`).
#[data]
#[serde(default, deny_unknown_fields)]
pub struct Catalog {
    /// Бюджет, % окна модели.
    pub budget_percent: u8,
    pub max_tokens: u32,
    pub line_max_tokens: u32,
    /// Reserved в группе, с которых она сворачивается в строку.
    pub collapse_from: u32,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            budget_percent: 2,
            max_tokens: 4000,
            line_max_tokens: 30,
            collapse_from: 5,
        }
    }
}

/// Warm-набор (`02 §5.4`): пересчитывает сон.
#[data]
#[serde(default, deny_unknown_fields)]
pub struct Warm {
    pub window: HumanDuration,
    pub half_life: HumanDuration,
    pub weights: Weights,
    pub load_weight: f32,
    pub thin_turns: u32,
    pub hysteresis: f32,
    pub new_tool_grace: HumanDuration,
    pub pin_loads: u32,
    pub pin_window: HumanDuration,
    pub pin_for: HumanDuration,
    pub max_hide_per_night: u32,
    /// Пауза, после которой удаления применяются компакцией `reset` (`02 §5.5`).
    pub apply_idle: HumanDuration,
}

impl Default for Warm {
    fn default() -> Self {
        const DAY: u64 = 24 * 60 * 60;
        Self {
            window: secs(30 * DAY),
            half_life: secs(7 * DAY),
            weights: Weights::default(),
            load_weight: 3.0,
            thin_turns: 10,
            hysteresis: 1.2,
            new_tool_grace: secs(7 * DAY),
            pin_loads: 2,
            pin_window: secs(7 * DAY),
            pin_for: secs(30 * DAY),
            max_hide_per_night: 8,
            apply_idle: secs(60 * 60),
        }
    }
}

#[data]
#[serde(default, deny_unknown_fields)]
pub struct Weights {
    pub chat: f32,
    pub folder: f32,
    pub global: f32,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            chat: 1.0,
            folder: 0.5,
            global: 0.25,
        }
    }
}

const fn secs(n: u64) -> HumanDuration {
    HumanDuration(Duration::from_secs(n))
}
