//! Счёт токенов (`01 §9`): якорь usage плюс оценка событий после него,
//! калибровка `bytes_per_token` по ответам, `max_tokens` шага (`01 §8`).

use eva_protocol::types::{
    content::Block as ContentBlock,
    generate::{Block, Message, ResultBlock, Usage},
    model::ModelDecl,
};

use crate::{
    config::ContextConfig,
    db::turns::Anchor,
    subsystems::turns::{
        log::{Event, Payload, ResultContent, ThinkingBlob},
        marker,
        render::Rendered,
    },
};

/// Границы калиброванного ratio.
const RATIO_MIN: f64 = 1.0;
const RATIO_MAX: f64 = 10.0;
/// Служебная строка рендера (маркер спикера, `[file …]`) для оценки до рендера.
const MARKER_BYTES: usize = 48;

/// Мера одного рендера или события.
#[derive(Debug, Clone, Copy)]
pub struct Estimator {
    /// `bytes_per_token` модели.
    pub ratio: f64,
    pub image_tokens: u32,
}

impl Estimator {
    pub fn new(ratio: Option<f64>, cfg: &ContextConfig) -> Self {
        Self {
            ratio: ratio
                .unwrap_or(cfg.ratio_default)
                .clamp(RATIO_MIN, RATIO_MAX),
            image_tokens: cfg.image_tokens,
        }
    }

    pub fn text(&self, bytes: usize) -> u32 {
        let tokens = (bytes as f64 / self.ratio).ceil();
        if tokens >= f64::from(u32::MAX) {
            u32::MAX
        } else {
            tokens as u32
        }
    }

    pub fn request(&self, rendered: &Rendered) -> u32 {
        self.prefix(rendered)
            .saturating_add(self.messages(&rendered.messages))
    }

    /// `system + tools`: точный счёт провайдера заменяет эту часть (`context.count_prefix`).
    pub fn prefix(&self, rendered: &Rendered) -> u32 {
        let mut bytes = rendered.system.iter().map(|s| s.text.len()).sum::<usize>();
        for tool in &rendered.tools {
            bytes += tool.name.len() + tool.description.len() + tool.input_schema.to_string().len();
        }
        self.text(bytes)
    }

    pub fn messages(&self, messages: &[Message]) -> u32 {
        let mut total = 0_u32;
        for message in messages {
            let (Message::User { blocks }
            | Message::Assistant { blocks }
            | Message::System { blocks, .. }) = message;
            for block in blocks {
                total = total.saturating_add(self.block(block));
            }
        }
        total
    }

    pub fn block(&self, block: &Block) -> u32 {
        match block {
            Block::Text { text } => self.text(text.len()),
            Block::Image { .. } => self.image_tokens,
            Block::ToolUse { name, input, .. } => self.text(name.len() + input.to_string().len()),
            Block::ToolResult { content, .. } => content
                .iter()
                .map(|c| match c {
                    ResultBlock::Text { text } => self.text(text.len()),
                    ResultBlock::Image { .. } => self.image_tokens,
                })
                .fold(0, u32::saturating_add),
            Block::Thinking { text, .. } => self.text(text.as_ref().map_or(0, String::len)),
            Block::RedactedThinking { .. } => 0,
        }
    }

    /// Вклад события в запрос после якоря; события без рендера — 0.
    pub fn event(&self, event: &Event) -> u32 {
        match &event.payload {
            Payload::Message(m) => {
                if m.consumed_by.is_some() {
                    return 0;
                }
                let marker = if m.speaker.is_some() {
                    self.text(MARKER_BYTES)
                } else {
                    0
                };
                m.blocks
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { text } => self.text(text.len()),
                        ContentBlock::Image { .. } => self.image_tokens,
                        ContentBlock::File { .. } => self.text(MARKER_BYTES),
                    })
                    .fold(marker, u32::saturating_add)
            }
            Payload::Thinking(t) => match &t.blob {
                ThinkingBlob::Signed { text, .. } => {
                    self.text(text.as_ref().map_or(0, String::len))
                }
                ThinkingBlob::Redacted { .. } => 0,
            },
            Payload::ToolUse(u) => self.text(u.name.len() + u.input.to_string().len()),
            Payload::ToolResult(r) => {
                let header = marker::tool_header(r.tseq).len() + 1;
                let body = match r.dedup_of {
                    Some(of) => self.text(marker::same_as(of).len()),
                    None => r
                        .content
                        .iter()
                        .map(|c| match c {
                            ResultContent::Text { text } => self.text(text.len()),
                            ResultContent::Image { .. } => self.image_tokens,
                            ResultContent::File { .. } => 32,
                        })
                        .fold(0, u32::saturating_add),
                };
                self.text(header).saturating_add(body)
            }
            Payload::SystemText(s) => self.text(s.text.len()),
            Payload::NoteUpdated(n) => self.text(marker::note_updated(n).len()),
            Payload::MemoryUpdated(m) => self.text(marker::memory_updated(m).len()),
            Payload::Reminder(r) => self.text(r.text.len()),
            Payload::ToolsAdded(t) => self.text(
                t.tools
                    .iter()
                    .map(|s| s.name.len() + s.description.len() + s.input_schema.to_string().len())
                    .sum(),
            ),
            Payload::ToolsRemoved(_)
            | Payload::ParamsChanged(_)
            | Payload::SegmentReset(_)
            | Payload::TurnEnded(_) => 0,
        }
    }
}

/// Размер запроса шага: якорь + события после него; без якоря — весь рендер,
/// `prefix_exact` (точный счёт `system+tools`) заменяет оценку префикса.
pub fn estimate(
    est: &Estimator,
    anchor: Option<Anchor>,
    events: &[Event],
    rendered: &Rendered,
    prefix_exact: Option<u32>,
) -> u32 {
    match anchor {
        Some(anchor) => events
            .iter()
            .filter(|e| e.id > anchor.event)
            .map(|e| est.event(e))
            .fold(anchor.tokens, u32::saturating_add),
        None => prefix_exact
            .unwrap_or_else(|| est.prefix(rendered))
            .saturating_add(est.messages(&rendered.messages)),
    }
}

/// Якорь после ответа: `input + cache_read + cache_write + output`.
pub fn anchor_tokens(usage: &Usage) -> u32 {
    let total = usage.input
        + usage.cache_read
        + usage.cache_write_short
        + usage.cache_write_long
        + usage.output;
    u32::try_from(total).unwrap_or(u32::MAX)
}

/// Байты текста запроса без картинок и число картинок — вход калибровки.
pub fn text_bytes(rendered: &Rendered) -> (usize, u32) {
    let mut bytes = rendered.system.iter().map(|s| s.text.len()).sum::<usize>();
    for tool in &rendered.tools {
        bytes += tool.name.len() + tool.description.len() + tool.input_schema.to_string().len();
    }
    let mut images = 0_u32;
    for message in &rendered.messages {
        let (Message::User { blocks }
        | Message::Assistant { blocks }
        | Message::System { blocks, .. }) = message;
        for block in blocks {
            match block {
                Block::Text { text } => bytes += text.len(),
                Block::Image { .. } => images += 1,
                Block::ToolUse { name, input, .. } => bytes += name.len() + input.to_string().len(),
                Block::ToolResult { content, .. } => {
                    for c in content {
                        match c {
                            ResultBlock::Text { text } => bytes += text.len(),
                            ResultBlock::Image { .. } => images += 1,
                        }
                    }
                }
                Block::Thinking { text, .. } => bytes += text.as_ref().map_or(0, String::len),
                Block::RedactedThinking { .. } => {}
            }
        }
    }
    (bytes, images)
}

/// Калибровка по ответу (`01 §9` п.4): `None` — образец короток, ratio не меняется.
pub fn calibrate(
    ratio: f64,
    text_bytes: usize,
    images: u32,
    usage: &Usage,
    cfg: &ContextConfig,
) -> Option<f64> {
    let input_total =
        usage.input + usage.cache_read + usage.cache_write_short + usage.cache_write_long;
    if input_total < u64::from(cfg.ratio_min_sample) {
        return None;
    }
    let denominator = input_total.saturating_sub(u64::from(images) * u64::from(cfg.image_tokens));
    if denominator < u64::from(cfg.ratio_min_sample) {
        return None;
    }
    let observed = text_bytes as f64 / denominator as f64;
    let w = cfg.ratio_ema_weight.clamp(0.0, 1.0);
    Some(((1.0 - w) * ratio + w * observed).clamp(RATIO_MIN, RATIO_MAX))
}

/// Окно вывода шага; `Err` — остаток меньше `min_output_tokens`: компакция до шага (`01 §6.1`).
pub fn max_tokens(model: &ModelDecl, estimate: u32, cfg: &ContextConfig) -> Result<u32, NoRoom> {
    let room = model
        .context_window
        .saturating_sub(estimate)
        .saturating_sub(cfg.output_margin);
    if room < cfg.min_output_tokens {
        return Err(NoRoom { room });
    }
    Ok(room.min(model.max_output))
}

/// Окна под вывод не хватает.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoRoom {
    pub room: u32,
}

/// Порог компакции в токенах: `compact_at × context_window`.
pub fn compact_threshold(model: &ModelDecl, compact_at: f32) -> u32 {
    let share = f64::from(compact_at.clamp(0.0, 1.0));
    (f64::from(model.context_window) * share) as u32
}

#[cfg(test)]
mod tests {
    use eva_protocol::types::{
        ModelId,
        generate::{Block, Message, SystemSection, ToolSpec},
        model::ModelCaps,
    };

    use super::*;

    fn cfg() -> ContextConfig {
        ContextConfig::default()
    }

    fn model(window: u32, max_output: u32) -> ModelDecl {
        ModelDecl {
            name: ModelId::from("m"),
            context_window: window,
            max_output,
            caps: ModelCaps::default(),
            price: None,
            embed: None,
        }
    }

    fn rendered(texts: &[&str], images: u32) -> Rendered {
        let mut blocks: Vec<Block> = texts
            .iter()
            .map(|t| Block::Text {
                text: (*t).to_owned(),
            })
            .collect();
        for _ in 0..images {
            blocks.push(Block::Image {
                media_type: "image/png".into(),
                data: vec![0_u8; 4096].into(),
            });
        }
        Rendered {
            system: vec![SystemSection {
                name: "persona".into(),
                text: "[persona]\nEva".into(),
            }],
            tools: vec![ToolSpec {
                name: "fs_read".into(),
                description: "Read a file".into(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
                deferred: false,
            }],
            messages: vec![Message::User { blocks }],
        }
    }

    #[test]
    fn estimate_within_30pct_including_image_fixture() {
        let cfg = cfg();
        let est = Estimator::new(None, &cfg);
        let text = "x".repeat(4000);
        let r = rendered(&[&text], 1);
        // «usage» фикстуры: байты текста / 4 и одна картинка по тарифу конфига
        let (bytes, images) = text_bytes(&r);
        let usage = (bytes as f64 / 4.0).ceil() as u32 + images * cfg.image_tokens;
        let got = est.request(&r);
        let ratio = f64::from(got) / f64::from(usage);
        assert!(
            (0.7..=1.3).contains(&ratio),
            "estimate {got} vs usage {usage}"
        );
        assert_eq!(estimate(&est, None, &[], &r, None), got);
        assert_eq!(
            estimate(&est, None, &[], &r, Some(10)),
            10 + est.messages(&r.messages)
        );
    }

    #[test]
    fn ratio_calibration_excludes_images_and_clamps() {
        let cfg = cfg();
        let text = "x".repeat(8000);
        let r = rendered(&[&text], 1);
        let (bytes, images) = text_bytes(&r);
        assert_eq!(images, 1);
        let usage = Usage {
            input: 2000 + u64::from(cfg.image_tokens),
            ..Usage::default()
        };
        let next = calibrate(4.0, bytes, images, &usage, &cfg).expect("sample is big enough");
        assert!((next - 4.0).abs() < 0.05, "ratio drifted to {next}");

        // та же картинка, не вычтенная из знаменателя, занизила бы ratio
        let naive = bytes as f64 / (usage.input as f64);
        assert!(naive < 3.0, "{naive}");

        let short = Usage {
            input: 100,
            ..Usage::default()
        };
        assert_eq!(calibrate(4.0, 400, 0, &short, &cfg), None);

        let huge = Usage {
            input: 1000,
            ..Usage::default()
        };
        let mut ratio = 4.0;
        for _ in 0..200 {
            ratio = calibrate(ratio, 1_000_000, 0, &huge, &cfg).expect("sample");
        }
        assert_eq!(ratio, RATIO_MAX);
        for _ in 0..200 {
            ratio = calibrate(ratio, 10, 0, &huge, &cfg).expect("sample");
        }
        assert_eq!(ratio, RATIO_MIN);
    }

    #[test]
    fn max_tokens_never_exceeds_window() {
        let cfg = cfg();
        let m = model(200_000, 64_000);
        for estimate in [0_u32, 50_000, 140_000, 190_000, 200_000, u32::MAX] {
            match max_tokens(&m, estimate, &cfg) {
                Ok(tokens) => {
                    assert!(tokens <= m.max_output);
                    assert!(
                        u64::from(estimate) + u64::from(tokens) + u64::from(cfg.output_margin)
                            <= u64::from(m.context_window),
                        "estimate {estimate} + {tokens} overflows"
                    );
                }
                Err(NoRoom { room }) => assert!(room < cfg.min_output_tokens),
            }
        }
        // Haiku-подобный случай: 0.7 × 200k + 64k > 200k — окно режет вывод
        assert_eq!(max_tokens(&m, 140_000, &cfg), Ok(200_000 - 140_000 - 1024));
    }
}
