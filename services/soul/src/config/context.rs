//! Секция `context` (`01`), компонент `turns`: system-секции, компакция,
//! кэш, счёт токенов. Пороги — токены и время (I9): ключ `*_chars` отвергается.

use std::{path::PathBuf, time::Duration};

use eva::{data, time::HumanDuration};

#[data]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    /// Секция 1 system, путь относительно `state_dir`; файла нет — ошибка старта.
    pub persona_file: PathBuf,
    /// BCP-47: язык summary, заметок и записей памяти.
    pub master_language: String,
    /// Порог компакции — доля окна модели под весь запрос; перекрывается surface/folder/chat.
    pub compact_at: f32,
    pub summary_tokens: u32,
    /// Запас `max_tokens` компакции под обязательный thinking.
    pub summary_reasoning_headroom: u32,
    /// Токенов `tool_result` во входе холодного пути.
    pub summary_result_tokens: u32,
    /// Доля `summary_tokens` под дословный хвост реплик хозяина.
    pub verbatim_share: f32,
    /// То же при аварийной компакции без модели.
    pub verbatim_share_fallback: f32,
    /// Downshift: `summary_tokens = min(summary_tokens, window_new × share)`.
    pub downshift_summary_share: f32,
    /// Резерв окна под вывод при расчёте `max_tokens`.
    pub output_margin: u32,
    /// Остаток окна меньше — компакция до шага.
    pub min_output_tokens: u32,
    pub context_editing: ContextEditing,
    pub cache: CacheConfig,
    /// Оценка картинки в токенах.
    pub image_tokens: u32,
    /// Стартовый `bytes_per_token` до калибровки.
    pub ratio_default: f64,
    pub ratio_ema_weight: f64,
    /// Образец калибровки короче — пропускается.
    pub ratio_min_sample: u32,
    /// Точный счёт `system+tools` на старте сегмента при `ProviderDecl.count_tokens`.
    pub count_prefix: bool,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            persona_file: PathBuf::from("persona.md"),
            master_language: String::from("en"),
            compact_at: 0.7,
            summary_tokens: 3000,
            summary_reasoning_headroom: 2000,
            summary_result_tokens: 40,
            verbatim_share: 0.15,
            verbatim_share_fallback: 0.5,
            downshift_summary_share: 0.1,
            output_margin: 1024,
            min_output_tokens: 4096,
            context_editing: ContextEditing::default(),
            cache: CacheConfig::default(),
            image_tokens: 1600,
            ratio_default: 4.0,
            ratio_ema_weight: 0.2,
            ratio_min_sample: 500,
            count_prefix: true,
        }
    }
}

/// Provider-native очистка старых результатов (`01 §5`).
#[data]
#[serde(default, deny_unknown_fields)]
pub struct ContextEditing {
    pub enabled: bool,
    /// Доля окна, с которой провайдер начинает чистить.
    pub trigger: f32,
    /// Последних `tool_result`, которые не трогать.
    pub keep: u32,
    /// Токенов за одну очистку, меньше — не чистить.
    pub clear_at_least: u32,
}

impl Default for ContextEditing {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger: 0.4,
            keep: 6,
            clear_at_least: 20_000,
        }
    }
}

/// Breakpoints, TTL по gap чата, keep-alive (`01 §7`).
#[data]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    /// Промежуточный breakpoint каждые N позиций `messages`.
    pub breakpoint_every: u32,
    pub gap_ema_weight: f64,
    /// Gap от этого порога — `Long` либо keep-alive.
    pub long_gap: HumanDuration,
    /// Gap от этого порога — снова `Short`.
    pub keep_alive_max: HumanDuration,
    pub keep_alive: bool,
    pub keep_alive_period: HumanDuration,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            breakpoint_every: 15,
            gap_ema_weight: 0.3,
            long_gap: HumanDuration(Duration::from_secs(5 * 60)),
            keep_alive_max: HumanDuration(Duration::from_secs(60 * 60)),
            keep_alive: true,
            keep_alive_period: HumanDuration(Duration::from_secs(4 * 60 + 30)),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn thresholds_are_tokens_not_chars() {
        let err = serde_json::from_value::<ContextConfig>(json!({"summary_chars": 12000}))
            .expect_err("a *_chars key is refused");
        assert!(err.to_string().contains("summary_chars"), "{err}");
        let cfg: ContextConfig =
            serde_json::from_value(json!({"summary_tokens": 1200})).expect("tokens are accepted");
        assert_eq!(cfg.summary_tokens, 1200);
        assert_eq!(cfg.cache.breakpoint_every, 15);
    }
}
