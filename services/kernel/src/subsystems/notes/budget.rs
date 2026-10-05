//! Бюджет и заполненность блокнота (`04 §2`): всё в токенах по
//! калиброванному ratio модели чата.

use eva_protocol::types::note::Fill;

use crate::{
    config::NotesConfig, db::notes::NoteRow, handling::inference::estimate_bytes,
    subsystems::notes::render,
};

/// `min(context_window · budget_percent / 100, budget_max_tokens)`; модели
/// нет в реестре — потолок, не отказ.
pub fn budget(context_window: Option<u32>, cfg: &NotesConfig) -> u32 {
    let ceiling = cfg.budget_max_tokens;
    context_window.map_or(ceiling, |window| {
        let share = u64::from(window) * u64::from(cfg.budget_percent.into_inner()) / 100;
        u32::try_from(share).map_or(ceiling, |s| s.min(ceiling))
    })
}

/// `inline_tokens` — весь inline-рендер, включая имена и закреплённые.
pub fn fill(notes: &[NoteRow], budget: u32, ratio: Option<f64>) -> Fill {
    Fill {
        tokens: estimate_bytes(render::inline(notes).len(), ratio),
        budget,
        count: u32::try_from(notes.len()).unwrap_or(u32::MAX),
    }
}

pub fn tokens(text: &str, ratio: Option<f64>) -> u32 {
    estimate_bytes(text.len(), ratio)
}
