//! Секция `memory` (`03 §11`): все ключи главы с дефолтами; доли и
//! бюджеты проверяются на старте (`Config::check`).

use std::time::Duration;

use eva::{data, time::HumanDuration};

use eva_protocol::types::ModelId;

const fn days(n: u64) -> HumanDuration {
    HumanDuration(Duration::from_secs(n * 86_400))
}

const fn millis(n: u64) -> HumanDuration {
    HumanDuration(Duration::from_millis(n))
}

#[data]
#[serde(default)]
pub struct MemoryConfig {
    /// Имя провайдера из `llm.providers`; `None` — первый сконфигурированный с
    /// embed-моделью и `local`.
    pub embedder: Option<String>,
    pub embed: EmbedConfig,
    pub salience: SalienceConfig,
    pub reinforce: ReinforceConfig,
    pub recall: RecallConfig,
    /// Весь hot-блок, токенов.
    pub budget_tokens: u32,
    pub hot: HotConfig,
    pub dedup: DedupConfig,
    /// `fact|event|core`: отказ тула сверх, не обрезание.
    pub text_max_tokens: u32,
    pub procedure: ProcedureConfig,
    /// GC superseded-версий (`08 §3`).
    pub history_ttl: HumanDuration,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            embedder: None,
            embed: EmbedConfig::default(),
            salience: SalienceConfig::default(),
            reinforce: ReinforceConfig::default(),
            recall: RecallConfig::default(),
            budget_tokens: 2000,
            hot: HotConfig::default(),
            dedup: DedupConfig::default(),
            text_max_tokens: 200,
            procedure: ProcedureConfig::default(),
            history_ttl: days(365),
        }
    }
}

impl MemoryConfig {
    /// Инварианты конфига: доли hot-блока и бюджет.
    pub fn check(&self) -> eyre::Result<()> {
        let HotConfig {
            core_share, split, ..
        } = &self.hot;
        eyre::ensure!(
            (0.0..=1.0).contains(core_share),
            "memory.hot.core_share must be within 0..=1, got {core_share}"
        );
        let sum = split.fact + split.event + split.procedure;
        eyre::ensure!(
            (sum - 1.0).abs() < 1e-3
                && split.fact >= 0.0
                && split.event >= 0.0
                && split.procedure >= 0.0,
            "memory.hot.split must sum to 1, got {sum}"
        );
        eyre::ensure!(
            self.budget_tokens > 0,
            "memory.budget_tokens must be positive"
        );
        eyre::ensure!(
            (0.0..=1.0).contains(&self.salience.floor),
            "memory.salience.floor must be within 0..=1"
        );
        eyre::ensure!(
            self.recall.limit >= 1 && self.recall.limit <= RecallConfig::LIMIT_MAX,
            "memory.recall.limit must be within 1..={}",
            RecallConfig::LIMIT_MAX
        );
        Ok(())
    }
}

#[data]
#[serde(default)]
pub struct EmbedConfig {
    /// `None` — первая embed-модель эмбеддера.
    pub model: Option<ModelId>,
    /// Ожидание эмбеддинга запроса recall; дольше — чистый FTS.
    pub query_timeout: HumanDuration,
    /// Ожидание эмбеддинга текста `remember` ради дедупа.
    pub write_timeout: HumanDuration,
    /// Строк за один вызов воркера backfill.
    pub batch: u32,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            model: None,
            query_timeout: millis(800),
            write_timeout: HumanDuration(Duration::from_secs(2)),
            batch: 64,
        }
    }
}

#[data]
#[serde(default)]
pub struct SalienceConfig {
    pub half_life: HalfLife,
    /// Нижняя граница decay.
    pub floor: f64,
    /// Вес `ln(1 + access_count)`.
    pub access_bonus: f64,
}

impl Default for SalienceConfig {
    fn default() -> Self {
        Self {
            half_life: HalfLife::default(),
            floor: 0.1,
            access_bonus: 0.1,
        }
    }
}

#[data]
#[serde(default)]
pub struct HalfLife {
    pub fact: HumanDuration,
    pub event: HumanDuration,
    pub procedure: HumanDuration,
}

impl Default for HalfLife {
    fn default() -> Self {
        Self {
            fact: days(180),
            event: days(30),
            procedure: days(365),
        }
    }
}

#[data]
#[serde(default)]
pub struct ReinforceConfig {
    /// Включение в hot-блок: `access_count +=`, возраст не сдвигается.
    pub passive: f64,
    /// Возврат `recall`/`read` и попадание в дубль.
    pub recall: f64,
}

impl Default for ReinforceConfig {
    fn default() -> Self {
        Self {
            passive: 0.1,
            recall: 1.0,
        }
    }
}

#[data]
#[serde(default)]
pub struct RecallConfig {
    /// Кандидатов из каждого источника до RRF.
    pub candidates: u32,
    pub rrf_k: f64,
    /// Относительный порог: отбросить `score < best · floor`.
    pub floor: f64,
    /// Дефолт `limit` тула; потолок — `LIMIT_MAX`.
    pub limit: u8,
}

impl RecallConfig {
    pub const LIMIT_MAX: u8 = 25;
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            candidates: 50,
            rrf_k: 60.0,
            floor: 0.15,
            limit: 8,
        }
    }
}

#[data]
#[serde(default)]
pub struct HotConfig {
    /// Потолок core от бюджета.
    pub core_share: f64,
    /// Доли остатка после core.
    pub split: Split,
    /// `importance`, с которой fact входит вне очереди.
    pub fact_guarantee: f64,
    pub event_fresh: HumanDuration,
    pub event_upcoming: HumanDuration,
    /// Годовщины: тот же день-месяц ±, `occurred_at < now − 300d`.
    pub event_anniversary: HumanDuration,
    /// Событий по теме.
    pub topic_k: u32,
    /// Последних реплик-речи в запрос темы.
    pub topic_replies: u32,
    pub topic_timeout: HumanDuration,
    /// Не чаще — уведомление хозяину о переборе core.
    pub core_overflow_notify: HumanDuration,
}

impl Default for HotConfig {
    fn default() -> Self {
        Self {
            core_share: 0.6,
            split: Split::default(),
            fact_guarantee: 0.8,
            event_fresh: days(7),
            event_upcoming: days(14),
            event_anniversary: days(3),
            topic_k: 4,
            topic_replies: 6,
            topic_timeout: millis(300),
            core_overflow_notify: days(1),
        }
    }
}

#[data]
#[serde(default)]
pub struct Split {
    pub fact: f64,
    pub event: f64,
    pub procedure: f64,
}

impl Default for Split {
    fn default() -> Self {
        Self {
            fact: 0.5,
            event: 0.3,
            procedure: 0.2,
        }
    }
}

#[data]
#[serde(default)]
pub struct DedupConfig {
    pub cos: f64,
    pub jaccard: f64,
    /// Соседей в ответе `remember`.
    pub neighbours: u32,
    /// Нормированный к лучшему RRF-ранг, с которого сосед показывается.
    pub neighbour_floor: f64,
}

impl Default for DedupConfig {
    fn default() -> Self {
        Self {
            cos: 0.92,
            jaccard: 0.7,
            neighbours: 3,
            neighbour_floor: 0.5,
        }
    }
}

#[data]
#[serde(default)]
pub struct ProcedureConfig {
    pub body_max_tokens: u32,
    pub description_max_tokens: u32,
}

impl Default for ProcedureConfig {
    fn default() -> Self {
        Self {
            body_max_tokens: 2000,
            description_max_tokens: 40,
        }
    }
}
