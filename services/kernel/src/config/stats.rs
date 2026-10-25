//! Секция `stats` (`09 §8`).

use eva::{data, time::HumanDuration};

use crate::config::gc::days;

#[data]
#[serde(default)]
pub struct StatsConfig {
    /// `tool_stats`, `chat_stats`, `health_stats`.
    pub keep: HumanDuration,
    /// `spend` живёт дольше: деньги.
    pub spend_keep: HumanDuration,
    pub cache: CacheConfig,
    /// Последних вызовов на RPC-метод для p50/p95.
    pub latency_ring: LatencyRing,
    pub tools: ToolsConfig,
}

impl Default for StatsConfig {
    fn default() -> Self {
        Self {
            keep: days(400),
            spend_keep: days(3 * 365),
            cache: CacheConfig::default(),
            latency_ring: LatencyRing::POS256,
            tools: ToolsConfig::default(),
        }
    }
}

#[eva::int(u16, 16..=65535)]
pub enum LatencyRing {}

#[data]
#[serde(default)]
pub struct CacheConfig {
    /// `cache_read / prefix_prev` ниже — разрыв кэша (`01 §11`).
    pub break_ratio: f32,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self { break_ratio: 0.9 }
    }
}

#[data]
#[derive(Default)]
#[serde(default)]
pub struct ToolsConfig {
    pub default_top: eva_protocol::requests::Top,
}
