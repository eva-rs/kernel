//! Компоненты `Supervisor` soul (`00 §4`, `11 §5`) и швы, которые им даёт
//! движок туров через `turns::driver::Driver`. Обратные швы — движок
//! зовёт память, заметки и тулы soul — `turns::ports`.

use eva::time::Timestamp;

use eva_protocol::types::ChatId;

use crate::components::Components;

pub mod cron;
pub mod gc;
pub mod memory;
pub mod modules;
pub mod notes;
pub mod search;
pub mod sleep;
pub mod stats;
pub mod titles;
pub mod tools;
pub mod turns;
pub mod work;

/// Тур джобы в её чате (`08 §1.3`).
pub trait CronTurns: Send + Sync + 'static {
    fn run(&self, req: cron::TurnRequest) -> impl Future<Output = cron::Outcome> + Send;
}

/// Туры фаз сна в чате `sleep` (`08 §2.2`).
pub trait SleepTurns<C: Components>: Send + Sync + 'static {
    /// Фаза 1: вызовы `sleep_memory_*` идут в `phase.call`, `sleep_done` закрывает тур.
    fn memory(
        &self,
        turn: sleep::PhaseTurn,
        phase: std::sync::Arc<tokio::sync::Mutex<sleep::memory::Phase<C>>>,
    ) -> impl Future<Output = sleep::Outcome> + Send;

    /// Фаза 2 (`08 §2.4`): vet и overlay групп — у реестра тулов.
    fn groups(
        &self,
        turn: sleep::PhaseTurn,
        phase: std::sync::Arc<tokio::sync::Mutex<sleep::groups::Phase<C>>>,
    ) -> impl Future<Output = sleep::Outcome> + Send;
}

/// Кооперативная отмена живых туров в чатах (`07 §8`).
pub trait TurnCancel: Send + Sync + 'static {
    fn cancel(&self, chats: Vec<ChatId>) -> impl Future<Output = ()> + Send;
}

/// Фаза 3 сна без модели (`08 §2.5`, `02 §5.4`).
pub trait WarmSets: Send + Sync + 'static {
    fn recompute(
        &self,
        now: Timestamp,
    ) -> impl Future<Output = Result<sleep::WarmOutcome, String>> + Send;
}
