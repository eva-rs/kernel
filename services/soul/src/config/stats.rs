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
            latency_ring: LatencyRing(256),
            tools: ToolsConfig::default(),
        }
    }
}

#[eva::int(newtype(n >= 16), not(schemars))]
pub struct LatencyRing(u16);

impl eva::_priv::schemars::JsonSchema for LatencyRing {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "LatencyRing".into()
    }

    fn json_schema(_: &mut eva::_priv::schemars::SchemaGenerator) -> eva::_priv::schemars::Schema {
        eva::_priv::schemars::json_schema!({
            "anyOf": [{"type": "integer", "minimum": 16, "maximum": 65535}]
        })
    }
}

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
