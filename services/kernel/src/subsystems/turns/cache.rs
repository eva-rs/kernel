//! Кэш промпта (`01 §7`): breakpoints по нормализованным `messages`, TTL по
//! gap чата, keep-alive. Чистые функции: решения принимает движок.

use eva::time::Timestamp;

use eva_protocol::types::{
    generate::{Block, CacheMode, GenerateRequest, Message, Position, Purpose},
    model::{ModelCaps, ModelDecl},
};

use crate::{config::context::CacheConfig, db::turns::Gap, subsystems::turns::render::Rendered};

/// Breakpoints (`01 §7.1`): конец `system`, хвост `messages`, промежуточные
/// каждые `every` позиций (серия tool_use/tool_result — одна позиция), не
/// больше `min(4, caps.cache_breakpoints)`; новый промежуточный вытесняет
/// самый старый. Префикс короче `cache_min_prefix` — без breakpoints.
pub fn place_breakpoints(
    rendered: &Rendered,
    caps: &ModelCaps,
    every: u32,
    estimate: u32,
) -> Vec<Position> {
    let limit = usize::from(caps.cache_breakpoints.min(4));
    if limit == 0 || rendered.messages.is_empty() || below_min_prefix(caps, estimate) {
        return Vec::new();
    }
    let last = rendered.messages.len() - 1;
    let tail = Position::Message {
        index: u32::try_from(last).unwrap_or(u32::MAX),
    };
    if limit == 1 {
        return vec![tail];
    }

    let mut positions = Vec::with_capacity(rendered.messages.len());
    let (mut p, mut prev_toolish) = (0_u32, false);
    for (i, message) in rendered.messages.iter().enumerate() {
        let toolish = is_toolish(message);
        if i == 0 || !(toolish && prev_toolish) {
            p += 1;
        }
        positions.push(p);
        prev_toolish = toolish;
    }
    let total = positions[last];
    let every = every.max(1);
    let mut intermediate: Vec<u32> = (1..)
        .map(|k| k * every)
        .take_while(|&q| q < total)
        .filter_map(|q| positions.iter().rposition(|&x| x == q))
        .filter(|&i| i < last)
        .map(|i| u32::try_from(i).unwrap_or(u32::MAX))
        .collect();
    let room = limit - 2;
    if intermediate.len() > room {
        intermediate.drain(..intermediate.len() - room);
    }

    let mut out = Vec::with_capacity(limit);
    out.push(Position::System);
    out.extend(
        intermediate
            .into_iter()
            .map(|index| Position::Message { index }),
    );
    out.push(tail);
    out
}

fn is_toolish(message: &Message) -> bool {
    match message {
        Message::Assistant { blocks } => blocks.iter().any(|b| matches!(b, Block::ToolUse { .. })),
        Message::User { blocks } => blocks.iter().any(|b| matches!(b, Block::ToolResult { .. })),
        Message::System { .. } => false,
    }
}

/// Префикс короче порога модели: кэш не пишется и разрывом не считается (D5).
pub fn below_min_prefix(caps: &ModelCaps, estimate: u32) -> bool {
    caps.cache_min_prefix > 0 && estimate < caps.cache_min_prefix
}

/// Стратегия последнего запроса тура по gap EMA (`01 §7.2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ttl {
    Short,
    /// `Short` и реплей каждые `keep_alive_period` до `keep_alive_max`.
    KeepAlive,
    Long,
}

pub fn ttl(gap: Gap, model: &ModelDecl, cfg: &CacheConfig) -> Ttl {
    let Some(ema) = gap.ema_ns else {
        return Ttl::Short;
    };
    let long_gap = cfg.long_gap.0.as_nanos() as f64;
    let max = cfg.keep_alive_max.0.as_nanos() as f64;
    if ema < long_gap || ema >= max {
        return Ttl::Short;
    }
    let period = cfg.keep_alive_period.0.as_nanos().max(1) as f64;
    let ticks = (ema.min(max) / period).ceil();
    let cheaper = model
        .price
        .as_ref()
        .is_some_and(|p| ticks * p.cache_read <= p.cache_write_long - p.cache_write_short);
    if cfg.keep_alive && model.caps.cache_keep_alive && cheaper {
        Ttl::KeepAlive
    } else {
        Ttl::Long
    }
}

/// `cache` запроса: промежуточные шаги тура — `Short`, последний — по `ttl`.
pub fn cache_mode(caps: &ModelCaps, estimate: u32, last: bool, ttl: Ttl) -> CacheMode {
    if caps.cache_breakpoints == 0 || below_min_prefix(caps, estimate) {
        return CacheMode::Off;
    }
    match (last, ttl) {
        (true, Ttl::Long) => CacheMode::Long,
        _ => CacheMode::Short,
    }
}

/// EMA gap'а start-to-start (`01 §7.2`), обновляется на первом шаге тура;
/// первый запрос чата gap не даёт.
pub fn update_gap(gap: Gap, now: Timestamp, cfg: &CacheConfig) -> Gap {
    let ema_ns = match gap.last_request_at {
        Some(last) if last < now => {
            let observed = (now.as_nanos().get() - last.as_nanos().get()) as f64;
            let w = cfg.gap_ema_weight.clamp(0.0, 1.0);
            Some(
                gap.ema_ns
                    .map_or(observed, |ema| (1.0 - w) * ema + w * observed),
            )
        }
        _ => gap.ema_ns,
    };
    Gap {
        ema_ns,
        last_request_at: Some(now),
    }
}

/// Keep-alive (`01 §7.3`): байт-идентичный реплей, отличие — `max_tokens: 0`.
pub fn keep_alive(last: &GenerateRequest) -> GenerateRequest {
    GenerateRequest {
        max_tokens: 0,
        purpose: Purpose::KeepAlive,
        ..last.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use eva::time::HumanDuration;
    use eva_protocol::types::{
        ModelId,
        generate::{ResultBlock, SystemSection},
        model::Price,
    };

    use super::*;

    fn caps(n: u8) -> ModelCaps {
        ModelCaps {
            cache_breakpoints: n,
            ..ModelCaps::default()
        }
    }

    fn text(s: &str) -> Block {
        Block::Text { text: s.into() }
    }

    fn user(i: usize) -> Message {
        Message::User {
            blocks: vec![text(&format!("u{i}"))],
        }
    }

    fn assistant(i: usize) -> Message {
        Message::Assistant {
            blocks: vec![text(&format!("a{i}"))],
        }
    }

    fn call(i: usize) -> [Message; 2] {
        [
            Message::Assistant {
                blocks: vec![Block::ToolUse {
                    id: format!("c{i}"),
                    name: "fs_read".into(),
                    input: serde_json::json!({}),
                }],
            },
            Message::User {
                blocks: vec![Block::ToolResult {
                    tool_use_id: format!("c{i}"),
                    content: vec![ResultBlock::Text {
                        text: "[t1]\nok".into(),
                    }],
                    is_error: false,
                }],
            },
        ]
    }

    fn rendered(messages: Vec<Message>) -> Rendered {
        Rendered {
            system: vec![SystemSection {
                name: "persona".into(),
                text: "[persona]\nEva".into(),
            }],
            tools: Vec::new(),
            messages,
        }
    }

    #[test]
    fn breakpoints_le_four_series_as_one_position_tail_only_moves() {
        // 41 обычное сообщение: промежуточные на позициях 15 и 30, хвост на 40
        let mut messages = Vec::new();
        for i in 0..20 {
            messages.push(user(i));
            messages.push(assistant(i));
        }
        messages.push(user(99));
        let bps = place_breakpoints(&rendered(messages), &caps(4), 15, 10_000);
        assert_eq!(
            bps,
            vec![
                Position::System,
                Position::Message { index: 14 },
                Position::Message { index: 29 },
                Position::Message { index: 40 },
            ]
        );

        // серия вызовов — одна позиция: 20 tool-раундов не двигают промежуточные
        let mut messages = vec![user(0)];
        for i in 0..20 {
            messages.extend(call(i));
        }
        messages.push(user(1));
        let bps = place_breakpoints(&rendered(messages), &caps(4), 15, 10_000);
        assert_eq!(bps, vec![Position::System, Position::Message { index: 41 }]);

        // внутри тура меняется только хвост
        let mut messages = Vec::new();
        for i in 0..16 {
            messages.push(user(i));
            messages.push(assistant(i));
        }
        messages.push(user(16));
        let short = place_breakpoints(&rendered(messages.clone()), &caps(4), 15, 10_000);
        messages.push(assistant(16));
        messages.push(user(17));
        let long = place_breakpoints(&rendered(messages), &caps(4), 15, 10_000);
        assert_eq!(short[..short.len() - 1], long[..long.len() - 1]);
        assert_ne!(short.last(), long.last());
        assert!(short.len() <= 4 && long.len() <= 4);

        assert!(place_breakpoints(&rendered(vec![user(0)]), &caps(0), 15, 10_000).is_empty());
        assert_eq!(
            place_breakpoints(&rendered(vec![user(0)]), &caps(1), 15, 10_000),
            vec![Position::Message { index: 0 }]
        );
        assert_eq!(
            place_breakpoints(
                &rendered(vec![user(0), assistant(0), user(1)]),
                &caps(2),
                15,
                10_000
            ),
            vec![Position::System, Position::Message { index: 2 }]
        );
        let short_prefix = ModelCaps {
            cache_breakpoints: 4,
            cache_min_prefix: 1024,
            ..ModelCaps::default()
        };
        assert!(place_breakpoints(&rendered(vec![user(0)]), &short_prefix, 15, 500).is_empty());
        assert_eq!(
            cache_mode(&short_prefix, 500, true, Ttl::Long),
            CacheMode::Off
        );
    }

    #[test]
    fn keep_alive_bytes_equal_except_max_tokens() {
        let req = GenerateRequest {
            model: ModelId::from("m"),
            system: vec![],
            tools: vec![],
            messages: vec![user(0)],
            breakpoints: vec![Position::System],
            cache: CacheMode::Short,
            effort: None,
            thinking: None,
            max_tokens: 1024,
            format: None,
            purpose: Purpose::Turn,
        };
        let alive = keep_alive(&req);
        assert_eq!(alive.max_tokens, 0);
        assert_eq!(alive.purpose, Purpose::KeepAlive);
        let back = GenerateRequest {
            max_tokens: 1024,
            purpose: Purpose::Turn,
            ..alive
        };
        assert_eq!(
            rmp_serde::to_vec_named(&back).unwrap(),
            rmp_serde::to_vec_named(&req).unwrap()
        );
    }

    #[test]
    fn gap_ema_moves_only_with_a_previous_request() {
        let cfg = CacheConfig::default();
        let none = Gap {
            ema_ns: None,
            last_request_at: None,
        };
        let g1 = update_gap(none, Timestamp::from_secs(1_000), &cfg);
        assert_eq!(g1.ema_ns, None);
        let g2 = update_gap(g1, Timestamp::from_secs(1_060), &cfg);
        assert_eq!(g2.ema_ns, Some(60e9));
        let g3 = update_gap(g2, Timestamp::from_secs(1_180), &cfg);
        assert_eq!(g3.ema_ns, Some(0.7 * 60e9 + 0.3 * 120e9));
    }

    #[test]
    fn ttl_by_gap_and_keep_alive_price() {
        let cfg = CacheConfig::default();
        let at = |mins: u64| Gap {
            ema_ns: Some(Duration::from_secs(mins * 60).as_nanos() as f64),
            last_request_at: None,
        };
        let model = |read: f64, keep_alive: bool| ModelDecl {
            name: ModelId::from("m"),
            context_window: 200_000,
            max_output: 8_000,
            caps: ModelCaps {
                cache_breakpoints: 4,
                cache_keep_alive: keep_alive,
                ..ModelCaps::default()
            },
            price: Some(Price {
                input: 1.0,
                output: 5.0,
                cache_read: read,
                cache_write_short: 1.25,
                cache_write_long: 2.0,
            }),
            embed: None,
        };
        let cheap = model(0.025, true);
        assert_eq!(ttl(at(1), &cheap, &cfg), Ttl::Short);
        assert_eq!(ttl(at(20), &cheap, &cfg), Ttl::KeepAlive);
        assert_eq!(ttl(at(90), &cheap, &cfg), Ttl::Short);
        // при read 0.1× keep-alive дороже часового TTL уже от ~35 мин
        assert_eq!(ttl(at(20), &model(0.1, true), &cfg), Ttl::KeepAlive);
        assert_eq!(ttl(at(40), &model(0.1, true), &cfg), Ttl::Long);
        assert_eq!(ttl(at(20), &model(0.025, false), &cfg), Ttl::Long);
        let off = CacheConfig {
            keep_alive: false,
            keep_alive_period: HumanDuration(Duration::from_secs(270)),
            ..CacheConfig::default()
        };
        assert_eq!(ttl(at(20), &cheap, &off), Ttl::Long);

        let caps = cheap.caps.clone();
        assert_eq!(
            cache_mode(&caps, 10_000, false, Ttl::Long),
            CacheMode::Short
        );
        assert_eq!(cache_mode(&caps, 10_000, true, Ttl::Long), CacheMode::Long);
        assert_eq!(
            cache_mode(&caps, 10_000, true, Ttl::KeepAlive),
            CacheMode::Short
        );
        assert_eq!(
            cache_mode(&ModelCaps::default(), 10_000, true, Ttl::Long),
            CacheMode::Off
        );
    }
}
