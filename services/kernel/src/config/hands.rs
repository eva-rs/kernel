//! Секция `hands` (`07 §9`): endpoint'ы рук — `Hands::local` или демон
//! `eva-hands` по Noise.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use eva::{data, time::HumanDuration};
use eva_hands::{Limits, Psk};

#[data]
#[derive(Default)]
#[serde(default)]
pub struct HandsConfig {
    pub endpoints: BTreeMap<String, EndpointConfig>,
    /// Endpoint чатов с `workdir_on = NULL`; нет в `endpoints` — рук у них нет.
    pub default_endpoint: Option<String>,
}

/// `{local: true, limits?}` либо `{address, key}`.
#[data]
#[serde(untagged)]
pub enum EndpointConfig {
    Local {
        /// Должен быть `true`: `false` не означает ни одной машины.
        local: bool,
        #[serde(default)]
        limits: HandsLimits,
    },
    Remote {
        address: String,
        key: Psk,
    },
}

/// `eva_hands::Limits` с длительностями `HumanDuration` вместо `*_secs`.
#[data]
#[serde(default)]
pub struct HandsLimits {
    pub max_read: u64,
    pub max_write: u64,
    pub max_hits: u64,
    pub max_output: u64,
    pub exec: bool,
    pub exec_timeout: HumanDuration,
    pub exec_max_timeout: HumanDuration,
    pub fetch: bool,
    /// Пустой список — вся ФС.
    pub roots: Vec<PathBuf>,
}

impl Default for HandsLimits {
    fn default() -> Self {
        Self::from(Limits::default())
    }
}

impl From<Limits> for HandsLimits {
    fn from(limits: Limits) -> Self {
        let Limits {
            max_read,
            max_write,
            max_hits,
            max_output,
            exec,
            exec_timeout_secs,
            exec_max_timeout_secs,
            fetch,
            roots,
        } = limits;

        Self {
            max_read,
            max_write,
            max_hits,
            max_output,
            exec,
            exec_timeout: HumanDuration(Duration::from_secs(exec_timeout_secs)),
            exec_max_timeout: HumanDuration(Duration::from_secs(exec_max_timeout_secs)),
            fetch,
            roots,
        }
    }
}

impl From<HandsLimits> for Limits {
    fn from(limits: HandsLimits) -> Self {
        let HandsLimits {
            max_read,
            max_write,
            max_hits,
            max_output,
            exec,
            exec_timeout,
            exec_max_timeout,
            fetch,
            roots,
        } = limits;

        Self {
            max_read,
            max_write,
            max_hits,
            max_output,
            exec,
            exec_timeout_secs: exec_timeout.as_secs(),
            exec_max_timeout_secs: exec_max_timeout.as_secs(),
            fetch,
            roots,
        }
    }
}
