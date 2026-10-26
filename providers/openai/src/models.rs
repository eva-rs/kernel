//! Реестр моделей: `GET /models` + поправки конфига → `ModelDecl`; нативные
//! флаги `01 §3` выключены по умолчанию — soul применяет fallback'и сам.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use eva_protocol::types::{
    Effort, ModelId,
    model::{ModelCaps, ModelDecl, Price},
};

use crate::config::{CapsOverride, Config, Dialect, ModelOverride};

/// `listing` — тело `GET /models` (`{"data": [...]}`); `None` — сервер его не дал.
pub fn declare(listing: Option<&Value>, config: &Config) -> Vec<ModelDecl> {
    let listed: BTreeMap<&str, &Value> = listing
        .and_then(|l| l.get("data"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| Some((m.get("id")?.as_str()?, m)))
        .collect();

    let names: BTreeSet<&str> = match &config.allow {
        Some(allow) => allow.iter().map(ModelId::as_str).collect(),
        None => listed
            .keys()
            .copied()
            .chain(config.models.keys().map(ModelId::as_str))
            .collect(),
    };

    let mut out = BTreeMap::new();
    for name in names {
        let listed = listed.get(name).copied();
        let over = config.models.get(&ModelId::from(name));
        if listed.is_none() && over.is_none() {
            log::warn!(model = name; "allowed model is neither listed by the server nor described in config");
            continue;
        }
        match decl(name, listed, over, config.dialect) {
            Some(decl) => {
                out.insert(decl.name.clone(), decl);
            }
            None => {
                log::warn!(model = name; "model skipped: context_window is unknown, set it in config")
            }
        }
    }
    out.into_values().collect()
}

fn decl(
    name: &str,
    listed: Option<&Value>,
    over: Option<&ModelOverride>,
    dialect: Dialect,
) -> Option<ModelDecl> {
    let default = ModelOverride::default();
    let over = over.unwrap_or(&default);
    let listed = listed.unwrap_or(&Value::Null);

    let context_window = over
        .context_window
        .or_else(|| context_window(listed))
        .or_else(|| over.embed.map(|e| e.max_input_tokens))?;
    let max_output = over
        .max_output
        .or_else(|| u32_at(listed, &["top_provider", "max_completion_tokens"]))
        .unwrap_or(if over.embed.is_some() {
            0
        } else {
            context_window
        });

    let family = Family::of(name);
    Some(ModelDecl {
        name: ModelId::from(name),
        context_window,
        max_output,
        caps: caps(listed, family, dialect, &over.caps),
        price: over.price.or_else(|| price(listed, family)),
        embed: over.embed,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    Anthropic,
    Gemini,
    OpenAi,
    Other,
}

impl Family {
    fn of(name: &str) -> Self {
        if name.starts_with("anthropic/") {
            Self::Anthropic
        } else if name.starts_with("google/gemini") {
            Self::Gemini
        } else if name.starts_with("openai/") {
            Self::OpenAi
        } else {
            Self::Other
        }
    }

    /// Семейства, которым OpenRouter велит ставить `cache_control` явно; у прочих кэш автоматический.
    fn cache_breakpoints(self) -> u8 {
        match self {
            Self::Anthropic | Self::Gemini => 4,
            Self::OpenAi | Self::Other => 0,
        }
    }

    /// Нижняя граница семейства: недооценка лишь тратит breakpoint впустую, переоценка теряла бы кэш.
    fn cache_min_prefix(self) -> u32 {
        match self {
            Self::Anthropic | Self::Gemini | Self::OpenAi => 1024,
            Self::Other => 0,
        }
    }
}

fn caps(listed: &Value, family: Family, dialect: Dialect, over: &CapsOverride) -> ModelCaps {
    let input = strings_at(listed, &["architecture", "input_modalities"]);
    let output = strings_at(listed, &["architecture", "output_modalities"]);
    let params = strings_at(listed, &["supported_parameters"]);
    let router = dialect == Dialect::Openrouter;

    let thinking = over.thinking.unwrap_or_else(|| {
        params
            .iter()
            .any(|p| matches!(*p, "reasoning" | "include_reasoning"))
    });
    let effort_levels = match &over.effort_levels {
        Some(levels) => levels.clone(),
        None if thinking => vec![Effort::Low, Effort::Medium, Effort::High],
        None => Vec::new(),
    };
    let cache_breakpoints = if router {
        over.cache_breakpoints.unwrap_or(family.cache_breakpoints())
    } else {
        0
    };

    ModelCaps {
        vision: over.vision.unwrap_or_else(|| input.contains(&"image")),
        image_out: over.image_out.unwrap_or_else(|| output.contains(&"image")),
        thinking,
        effort_levels,
        structured_output: over.structured_output.unwrap_or_else(|| {
            params
                .iter()
                .any(|p| matches!(*p, "structured_outputs" | "response_format"))
        }),
        system_in_messages: over.system_in_messages.unwrap_or(false),
        // Подпись thinking у Anthropic привязана к набору tools: soul не реплеит блоки до `tools_added`.
        thinking_binding: over
            .thinking_binding
            .unwrap_or(router && family == Family::Anthropic),
        cache_breakpoints,
        cache_min_prefix: over.cache_min_prefix.unwrap_or(if router {
            family.cache_min_prefix()
        } else {
            0
        }),
        ..ModelCaps::default()
    }
}

fn price(listed: &Value, family: Family) -> Option<Price> {
    let pricing = listed.get("pricing")?;
    let field = |key: &str| pricing.get(key).and_then(Value::as_str).and_then(per_mtok);
    let input = field("prompt")?;
    let output = field("completion")?;
    if input < 0.0 || output < 0.0 {
        return None;
    }
    let cache_write_short = field("input_cache_write").unwrap_or(input);
    Some(Price {
        input,
        output,
        cache_read: field("input_cache_read").unwrap_or(input),
        cache_write_short,
        // Часовая запись у Anthropic — 2× базового входа; OpenRouter отдаёт только пятиминутную.
        cache_write_long: if family == Family::Anthropic {
            input * 2.0
        } else {
            cache_write_short
        },
    })
}

/// Цена за токен строкой → за MTok сдвигом запятой: `0.000003 × 1e6` во float дал бы хвост.
fn per_mtok(raw: &str) -> Option<f64> {
    let raw = raw.trim();
    let (negative, digits) = match raw.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return raw.parse::<f64>().ok().map(|v| v * 1e6);
    }
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    let shift = frac.len().min(6);
    let mut shifted = format!("{int}{}", &frac[..shift]);
    shifted.extend(std::iter::repeat_n('0', 6 - shift));
    if frac.len() > 6 {
        shifted.push('.');
        shifted.push_str(&frac[6..]);
    }
    let value = shifted.parse::<f64>().ok()?;
    Some(if negative { -value } else { value })
}

fn context_window(listed: &Value) -> Option<u32> {
    u32_at(listed, &["context_length"])
        .or_else(|| u32_at(listed, &["top_provider", "context_length"]))
        .or_else(|| u32_at(listed, &["max_model_len"]))
}

fn at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter().try_fold(value, |v, key| v.get(key))
}

fn u32_at(value: &Value, path: &[&str]) -> Option<u32> {
    at(value, path)
        .and_then(Value::as_u64)
        .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
}

fn strings_at<'a>(value: &'a Value, path: &[&str]) -> Vec<&'a str> {
    at(value, path)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::per_mtok;

    #[test]
    fn per_mtok_shifts_the_decimal_point() {
        assert_eq!(per_mtok("0.000003"), Some(3.0));
        assert_eq!(per_mtok("0.0000003"), Some(0.3));
        assert_eq!(per_mtok("0.00000375"), Some(3.75));
        assert_eq!(per_mtok("0"), Some(0.0));
        assert_eq!(per_mtok("2"), Some(2_000_000.0));
        assert_eq!(per_mtok("-1"), Some(-1_000_000.0));
        assert_eq!(per_mtok(".5"), Some(500_000.0));
        assert_eq!(per_mtok("abc"), None);
    }
}
