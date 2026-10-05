//! Секция `notes` (`04 §2`): бюджеты блокнота в токенах; инвариант
//! `pinned_budget_tokens ≥ max_note_tokens` проверяет `Config::check`.

use eva::data;

#[data]
#[serde(default)]
pub struct NotesConfig {
    /// `false` — блокнота нет: тулы не регистрируются, `notes.list` отвечает `unavailable`.
    pub enabled: bool,
    /// Доля `context_window` модели чата.
    pub budget_percent: BudgetPercent,
    /// Абсолютный потолок бюджета.
    pub budget_max_tokens: u32,
    /// Потолок индекса: он в промпте каждого шага в `index`.
    pub index_max_tokens: u32,
    /// Потолок тела одной заметки.
    pub max_note_tokens: u32,
    /// Потолок числа заметок в чате.
    pub max_notes: u32,
    /// Штук, при превышении inline считается переполненным.
    pub inline_max_notes: u32,
    /// Master-туров в `squeeze` до падения в `index`.
    pub squeeze_turns: u32,
    /// Порог возврата `index → inline` по токенам.
    pub return_percent: ReturnPercent,
    pub pinned_max: u32,
    /// Σ тел закреплённых.
    pub pinned_budget_tokens: u32,
    /// Доля порога компакции, с которой в reminder идёт notice.
    pub pressure_notice_percent: ReturnPercent,
    /// За сколько токенов до порога компакции идёт wrap-up.
    pub wrap_up_reserve_tokens: u32,
}

impl Default for NotesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            budget_percent: BudgetPercent::POS1,
            budget_max_tokens: 4000,
            index_max_tokens: 1000,
            max_note_tokens: 600,
            max_notes: 40,
            inline_max_notes: 8,
            squeeze_turns: 2,
            return_percent: ReturnPercent::POS60,
            pinned_max: 3,
            pinned_budget_tokens: 800,
            pressure_notice_percent: ReturnPercent::POS60,
            wrap_up_reserve_tokens: 8000,
        }
    }
}

impl NotesConfig {
    /// Законная заметка обязана помещаться в булавку.
    pub fn check(&self) -> eyre::Result<()> {
        eyre::ensure!(
            self.pinned_budget_tokens >= self.max_note_tokens,
            "notes.pinned_budget_tokens ({}) must be at least notes.max_note_tokens ({})",
            self.pinned_budget_tokens,
            self.max_note_tokens
        );
        eyre::ensure!(
            self.max_notes >= 1 && self.inline_max_notes >= 1,
            "notes.max_notes and notes.inline_max_notes must be positive"
        );
        Ok(())
    }
}

#[eva::int(u8, 1..=50)]
pub enum BudgetPercent {}

/// Проценты порогов гистерезиса и давления.
#[eva::int(u8, 10..=99)]
pub enum ReturnPercent {}
