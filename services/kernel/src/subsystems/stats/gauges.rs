//! Живые gauge'и `09 §4`: величины в памяти, не счётчики; пишут компоненты,
//! читает `stats.health`.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use eva::{
    data,
    time::{HumanDuration, Timestamp},
};

use eva_protocol::types::{
    ModelId,
    generate::RateLimits,
    stats::{Latency, RateLimit},
};

pub struct Gauges {
    turns: Arc<AtomicU32>,
    hot_tokens: AtomicU32,
    rate_limits: Mutex<BTreeMap<ModelId, RateLimit>>,
    latency: Mutex<BTreeMap<String, VecDeque<Duration>>>,
    /// `stats.latency_ring`: последних вызовов на метод.
    ring: usize,
}

/// Снимок для `stats.health`.
#[data]
pub struct Live {
    pub turns_in_flight: u32,
    pub hot_tokens: u32,
    pub rate_limits: Vec<RateLimit>,
    pub latency: Vec<Latency>,
}

/// Тур в полёте, пока жив guard: отмена и паника тоже его снимают.
pub struct InFlight(Arc<AtomicU32>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Gauges {
    pub fn new(ring: usize) -> Self {
        Self {
            turns: Arc::default(),
            hot_tokens: AtomicU32::new(0),
            rate_limits: Mutex::default(),
            latency: Mutex::default(),
            ring: ring.max(1),
        }
    }

    /// движок туров держит guard на время тура.
    pub fn turn_started(&self) -> InFlight {
        self.turns.fetch_add(1, Ordering::Relaxed);
        InFlight(self.turns.clone())
    }

    /// Токены последнего собранного hot-блока (`03 §10`).
    pub fn set_hot_tokens(&self, tokens: u32) {
        self.hot_tokens.store(tokens, Ordering::Relaxed);
    }

    /// Последний снимок лимитов модели: per model, не на процесс.
    pub fn rate_limit(&self, model: &ModelId, limits: RateLimits, seen_at: Timestamp) {
        let RateLimits {
            requests_remaining,
            tokens_remaining,
            reset_at,
        } = limits;
        locked(&self.rate_limits).insert(
            model.clone(),
            RateLimit {
                model: model.clone(),
                requests_remaining,
                requests_limit: None,
                tokens_remaining,
                tokens_limit: None,
                reset_at,
                seen_at,
            },
        );
    }

    /// Длительность вызова: `tools.call` или `inference:<model>`.
    pub fn latency(&self, method: &str, took: Duration) {
        let mut rings = locked(&self.latency);
        let ring = rings.entry(method.to_owned()).or_default();
        if ring.len() == self.ring {
            ring.pop_front();
        }
        ring.push_back(took);
    }

    pub fn live(&self) -> Live {
        let latency = locked(&self.latency)
            .iter()
            .filter(|(_, ring)| !ring.is_empty())
            .map(|(method, ring)| {
                let mut sorted: Vec<Duration> = ring.iter().copied().collect();
                sorted.sort_unstable();
                Latency {
                    method: method.clone(),
                    p50: HumanDuration(percentile(&sorted, 50)),
                    p95: HumanDuration(percentile(&sorted, 95)),
                }
            })
            .collect();
        Live {
            turns_in_flight: self.turns.load(Ordering::Relaxed),
            hot_tokens: self.hot_tokens.load(Ordering::Relaxed),
            rate_limits: locked(&self.rate_limits).values().cloned().collect(),
            latency,
        }
    }
}

/// Ближайший ранг (nearest-rank) по отсортированной выборке.
fn percentile(sorted: &[Duration], p: usize) -> Duration {
    let rank = (sorted.len() * p).div_ceil(100).max(1);
    sorted.get(rank - 1).copied().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use super::*;

    #[test]
    fn in_flight_follows_guards() {
        let g = Gauges::new(4);
        let a = g.turn_started();
        let b = g.turn_started();
        assert_eq!(g.live().turns_in_flight, 2);
        drop(a);
        assert_eq!(g.live().turns_in_flight, 1);
        drop(b);
        assert_eq!(g.live().turns_in_flight, 0);
    }

    #[test]
    fn latency_ring_keeps_the_last_calls() {
        let g = Gauges::new(4);
        for ms in [900, 1, 2, 3, 4] {
            g.latency("tools.call", Duration::from_millis(ms));
        }
        let live = g.live();
        let l = &live.latency[0];
        assert_eq!(l.method, "tools.call");
        assert_eq!(
            (l.p50.0, l.p95.0),
            (Duration::from_millis(2), Duration::from_millis(4)),
            "900ms fell out"
        );
    }

    #[test]
    fn rate_limits_are_per_model() {
        let g = Gauges::new(4);
        let at = Timestamp::from_nanos(NonZeroU64::new(5).expect("non-zero"));
        let limits = |n| RateLimits {
            requests_remaining: Some(n),
            tokens_remaining: None,
            reset_at: None,
        };
        g.rate_limit(&ModelId::from("a"), limits(1), at);
        g.rate_limit(&ModelId::from("b"), limits(2), at);
        g.rate_limit(&ModelId::from("a"), limits(3), at);
        let live = g.live();
        let got: Vec<_> = live
            .rate_limits
            .iter()
            .map(|r| (r.model.as_str(), r.requests_remaining))
            .collect();
        assert_eq!(got, [("a", Some(3)), ("b", Some(2))]);
    }
}
