//! Секция `sleep` (`08 §2.7`).

use std::time::Duration;

use eva::{data, time::HumanDuration};

use eva_protocol::types::{Effort, ModelId};

use crate::config::wall::WallTime;

#[data]
#[serde(default)]
pub struct SleepConfig {
    pub enabled: bool,
    /// Граница ночи в `system.tz`.
    pub at: WallTime,
    pub tick: HumanDuration,
    /// Тишина перед стартом: ни живых туров вне `system`, ни свежих человеческих реплик.
    pub idle_for: HumanDuration,
    /// Старт без тишины через столько от границы.
    pub force_after: HumanDuration,
    /// `None` — роль `llm.roles.smartest`.
    pub model: Option<ModelId>,
    pub effort: Effort,
    /// Шагов на фазу; дальше тур отменяется.
    pub max_steps: u32,
    /// Vet и журнал без записей; фаза 3 применяется всегда.
    pub dry_run: bool,
    /// Фаза 1 пропускается при меньшем корпусе.
    pub min_memories: u32,
    pub keep_recent: HumanDuration,
    pub max_ops: u32,
    pub max_forget: u32,
    pub max_clusters: u32,
    pub cluster: ClusterConfig,
    pub forget: ForgetConfig,
    pub groups: GroupsConfig,
    pub notify: NotifyConfig,
}

impl Default for SleepConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            at: WallTime("00:00".parse().unwrap_or_else(|_| unreachable!())),
            tick: HumanDuration(Duration::from_secs(5 * 60)),
            idle_for: HumanDuration(Duration::from_secs(10 * 60)),
            force_after: HumanDuration(Duration::from_secs(3 * 60 * 60)),
            model: None,
            effort: Effort::High,
            max_steps: 60,
            dry_run: false,
            min_memories: 20,
            keep_recent: HumanDuration(Duration::from_secs(48 * 60 * 60)),
            max_ops: 40,
            max_forget: 8,
            max_clusters: 12,
            cluster: ClusterConfig::default(),
            forget: ForgetConfig::default(),
            groups: GroupsConfig::default(),
            notify: NotifyConfig::default(),
        }
    }
}

#[data]
#[serde(default)]
pub struct ClusterConfig {
    pub jaccard: f64,
    pub cosine: f64,
    /// Записей в кластере.
    pub max: u32,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            jaccard: 0.35,
            cosine: 0.6,
            max: 8,
        }
    }
}

/// Порог `forget_candidate` (`03 §3`); полураспады — `memory.salience.half_life`.
#[data]
#[serde(default)]
pub struct ForgetConfig {
    pub importance_max: f64,
}

impl Default for ForgetConfig {
    fn default() -> Self {
        Self {
            importance_max: 0.3,
        }
    }
}

#[data]
#[serde(default)]
pub struct GroupsConfig {
    pub max_ops: u32,
}

impl Default for GroupsConfig {
    fn default() -> Self {
        Self { max_ops: 10 }
    }
}

#[data]
#[serde(default)]
pub struct NotifyConfig {
    pub min_applied: u32,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self { min_applied: 5 }
    }
}
