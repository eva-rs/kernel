//! Крон: джобы и их запуски.

use eva::{
    data,
    time::{HumanDuration, Timestamp},
};

use crate::types::{ChatId, CronJobId, Effort, ModelId, RunId};

#[data]
pub struct Job {
    pub id: CronJobId,
    pub name: String,
    pub task: String,
    /// `None` — one-shot.
    pub interval: Option<HumanDuration>,
    /// `None` — one-shot уже запущен.
    pub next_fire_at: Option<Timestamp>,
    pub expires_at: Option<Timestamp>,
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
    /// Текущие каноничные имена whitelist; `None` — без ограничений.
    pub tools: Option<Vec<String>>,
    pub chat: ChatId,
    pub runs_total: u64,
    pub spent_total: f64,
    pub created_at: Timestamp,
}

#[data]
pub struct Run {
    pub run_id: RunId,
    /// Без ссылочной целостности: переживает удаление one-shot.
    pub job: CronJobId,
    pub started: Timestamp,
    pub finished: Option<Timestamp>,
    pub ok: Option<bool>,
    /// Последняя реплика ассистента, обрезанная.
    pub output: Option<String>,
    pub cost: Option<f64>,
}
