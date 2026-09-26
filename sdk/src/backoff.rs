//! Экспоненциальная пауза между попытками подключения.

use std::time::Duration;

pub const MIN: Duration = Duration::from_secs(1);
pub const MAX: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    min: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    /// `min` нулём не бывает: нулевая пауза крутила бы цикл вхолостую.
    pub fn new(min: Duration, max: Duration) -> Self {
        let min = min.max(Duration::from_millis(1));
        let max = max.max(min);
        Self {
            min,
            max,
            next: min,
        }
    }

    /// Пауза перед следующей попыткой; каждая следующая вдвое дольше, до `max`.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = self.next.saturating_mul(2).min(self.max);
        delay
    }

    /// Соединение удалось: следующий обрыв начинается с `min`.
    pub fn reset(&mut self) {
        self.next = self.min;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(MIN, MAX)
    }
}
