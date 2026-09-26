//! Секция `notes` (`04 §2`): ключи, нужные M1 (`notes.list`).

use eva::data;

#[data]
#[serde(default)]
pub struct NotesConfig {
    /// `false` — блокнота нет: `notes.list` отвечает `unavailable`.
    pub enabled: bool,
    /// Доля `context_window` модели чата.
    pub budget_percent: BudgetPercent,
    /// Абсолютный потолок бюджета.
    pub budget_max_tokens: u32,
}

impl Default for NotesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            budget_percent: BudgetPercent::POS1,
            budget_max_tokens: 4000,
        }
    }
}

#[eva::int(u8, 1..=50)]
pub enum BudgetPercent {}
