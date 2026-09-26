//! Режим размышления. Наследуется surface → folder → chat → message, как `Effort`.

use eva::data;

/// Модели с обязательным thinking игнорируют `Off` — решает провайдер.
#[data(copy, ord)]
#[derive(Hash, Default)]
pub enum Thinking {
    #[default]
    Adaptive,
    Off,
}
