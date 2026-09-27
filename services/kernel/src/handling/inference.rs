//! Хендлер `inference.complete` (`06 §5`, `11 §6`): разовый вызов модели
//! модулем или ядром — без чата, истории и тулов, `cache: off`, `effort` по
//! умолчанию `low`. Каждый usage-чанк — строка `spend` с
//! `source = module`, у ядра — `system` (`09 §2`).

use eva_core::{GenSink, Provider as _};
use eva_protocol::{
    errors::{self, Aux, AuxResult, ext::*},
    requests::{Response, inference::complete},
    types::{
        Effort, ModelId, ModuleId,
        content::Block,
        generate::{
            self, CacheMode, GenItem, GenerateRequest, Message, OutputFormat, ProviderErrorKind,
            Purpose, Stop, StopReason, SystemSection, Usage,
        },
        model::ModelEntry,
    },
};
use futures::{StreamExt as _, channel::mpsc};
use serde_json::Value;

use crate::{
    components::Components,
    config::LlmConfig,
    db::{
        Db as _, Queries as _,
        modules::{Cost, CostSource, ModuleSpend},
        stats::Metric,
    },
    registry::Models,
    state::{Policy, State},
};

/// `context.ratio_default` (`01 §9`): модель ещё не калибрована.
const RATIO_DEFAULT: f64 = 4.0;
/// `context.image_tokens` (`01 §9`).
const IMAGE_TOKENS: u32 = 1600;
/// `context.output_margin` (`01 §8`).
const OUTPUT_MARGIN: u32 = 1024;

/// `est(Text) = bytes / ratio(model)` (`01 §9`), вверх; ratio зажат в `[1, 10]`.
pub(crate) fn estimate_bytes(bytes: usize, ratio: Option<f64>) -> u32 {
    let tokens = (bytes as f64 / ratio.unwrap_or(RATIO_DEFAULT).clamp(1.0, 10.0)).ceil();
    if tokens >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        tokens as u32
    }
}

/// `llm.roles.cheap`, иначе самая дешёвая по `price.input` модель реестра,
/// без цен — `default_model` (`01 §8.2`).
fn cheap(models: &Models, llm: &LlmConfig) -> ModelId {
    if let Some(model) = &llm.roles.cheap {
        return model.clone();
    }
    models
        .iter()
        .filter(|m| m.decl.embed.is_none())
        .filter_map(|m| m.decl.price.map(|p| (p.input, &m.decl.name)))
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map_or_else(|| llm.default_model.clone(), |(_, name)| name.clone())
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Complete<C: Components>(
    state: &mut State<C>,
    args: complete::Args,
) -> Response<complete::Ok, complete::Err> {
    // `Policy` не нужна: объекты вызова — только blob'ы входа, их проверяет `render`.
    let module = state.caller.module().cloned();
    let complete::Args {
        model,
        system,
        input,
        schema,
        max_tokens,
        effort,
    } = args;
    (!input.is_empty()).true_or(Aux::Invalid("input is empty".into()))?;
    (max_tokens != Some(0)).true_or(Aux::Invalid("max_tokens must be positive".into()))?;

    let models = state.shared.models();
    let llm = &state.shared.cfg().llm;
    let model = model.unwrap_or_else(|| cheap(&models, llm));
    let entry = models
        .get(&model)
        .ok_or_else(|| Aux::NotFound(format!("model {model}")))?
        .clone();
    let provider = state.shared.provider(&entry.provider).ok_or_else(|| {
        Aux::Unavailable(format!(
            "provider {} of model {model} is not connected",
            entry.provider
        ))
    })?;

    let blocks = render(state, &entry, input).await?;
    let db = state.db_conn.acquire(&state.shared).await?;
    let key = model.clone();
    let ratio = db.run(move |c| c.turns().ratio(&key)).await?;
    state.db_conn.release();

    let mut sections = Vec::new();
    if !system.is_empty() {
        sections.push(SystemSection {
            name: String::from("system"),
            text: system,
        });
    }
    let structured = schema.is_some();
    let format = match schema {
        Some(schema) if entry.decl.caps.structured_output => Some(OutputFormat {
            schema,
            strict: true,
        }),
        // Без нативной формы схема идёт текстом, ответ разбирается как JSON.
        Some(schema) => {
            sections.push(SystemSection {
                name: String::from("output"),
                text: format!(
                    "Answer with a single JSON value, without prose or code fences, valid against this JSON Schema:\n{schema}"
                ),
            });
            None
        }
        None => None,
    };

    let estimate = estimate_input(&sections, &blocks, format.as_ref(), ratio);
    let max_tokens = output_budget(&entry, estimate, max_tokens)?;
    let req = GenerateRequest {
        model: model.clone(),
        system: sections,
        tools: Vec::new(),
        messages: vec![Message::User { blocks }],
        breakpoints: Vec::new(),
        cache: CacheMode::Off,
        effort: Some(effort.unwrap_or(Effort::Low)),
        thinking: None,
        max_tokens,
        format,
        purpose: Purpose::Complete,
    };

    let (sink, mut items) = mpsc::unbounded();
    let res = provider.generate(req, GenSink::new(sink)).await;
    items.close();
    let mut out = Collected::default();
    while let Some(item) = items.next().await {
        out.push(item);
    }

    let failed = res.as_ref().err().map(|e| e.kind);
    record::<C>(state, &entry, module.as_ref(), &out.chunks, failed).await;
    if let Err(error) = res {
        return Err(errors::inference::Provider { error }.spec());
    }

    let text = (!out.text.is_empty()).then_some(out.text);
    let json = if structured {
        text.as_deref().and_then(parse_json)
    } else {
        None
    };
    Ok(complete::Ok {
        text,
        json,
        stop: out.stop.unwrap_or_else(|| Stop {
            reason: StopReason::Other,
            details: Some(String::from("stream ended without stop")),
        }),
        usage: out.usage,
    })
}

/// Оценка входа запроса (`01 §9`): текст — байты через ratio, картинка — фикс.
fn estimate_input(
    sections: &[SystemSection],
    blocks: &[generate::Block],
    format: Option<&OutputFormat>,
    ratio: Option<f64>,
) -> u32 {
    let mut bytes: usize = sections.iter().map(|s| s.text.len()).sum();
    let mut images: u32 = 0;
    for block in blocks {
        match block {
            generate::Block::Text { text } => bytes += text.len(),
            _ => images = images.saturating_add(1),
        }
    }
    if let Some(format) = format {
        bytes += format.schema.to_string().len();
    }
    estimate_bytes(bytes, ratio).saturating_add(images.saturating_mul(IMAGE_TOKENS))
}

/// `max_tokens = min(запрошенное или max_output, context_window − оценка входа −
/// output_margin)` (`01 §8`): иначе провайдер отвечает 400 на `input + max_tokens > окно`.
fn output_budget(entry: &ModelEntry, estimate: u32, requested: Option<u32>) -> AuxResult<u32> {
    let window = entry.decl.context_window;
    let room = window
        .saturating_sub(estimate)
        .saturating_sub(OUTPUT_MARGIN);
    if room == 0 {
        return Err(Aux::Invalid(format!(
            "input is about {estimate} tokens and does not fit model {} ({window}-token window, {OUTPUT_MARGIN} reserved for output)",
            entry.decl.name
        )));
    }
    Ok(requested
        .unwrap_or(entry.decl.max_output)
        .min(entry.decl.max_output)
        .min(room))
}

/// Вход модуля → блоки запроса: картинка — байтами при `vision`, иначе и
/// файл — маркером реестра I13 (`07 §6`).
async fn render<C: Components>(
    state: &mut State<C>,
    entry: &ModelEntry,
    input: Vec<Block>,
) -> Response<Vec<generate::Block>, complete::Err> {
    let State {
        shared,
        world,
        caller,
        db_conn,
        ..
    } = state;
    let policy = Policy::new(caller);
    let mut out = Vec::with_capacity(input.len());

    for block in input {
        let id = match block {
            Block::Text { text } => {
                out.push(generate::Block::Text { text });
                continue;
            }
            Block::Image { ref blob, .. } | Block::File { ref blob } => blob.clone(),
        };
        let db = db_conn.acquire(shared).await?;
        let key = id.clone();
        let row = db
            .run(move |c| c.blobs().get(&key))
            .await?
            .filter(|row| row.expires_at > world.now())
            .ok_or_else(|| Aux::NotFound(format!("blob {id}")))?;
        match &row.chat {
            Some(chat) => policy.chats().read(chat)?,
            None => policy.owner()?,
        }

        out.push(match block {
            Block::Image { media_type, .. } if entry.decl.caps.vision => {
                let path = shared
                    .cfg()
                    .blob_dir()
                    .join(row.id.as_str())
                    .join(&row.name);
                let data = tokio::fs::read(&path)
                    .await
                    .map_err(|e| Aux::internal(format_args!("blob store: read {id}: {e}")))?;
                generate::Block::Image {
                    media_type,
                    data: data.into(),
                }
            }
            Block::Image { .. } => generate::Block::Text {
                text: format!("[image {id}] {}", row.name),
            },
            _ => generate::Block::Text {
                text: format!("[file {id}] {} ({} bytes)", row.name, row.size),
            },
        });
    }
    Ok(out)
}

#[derive(Default)]
struct Collected {
    text: String,
    stop: Option<Stop>,
    /// Сумма чанков для `Ok.usage`.
    usage: Usage,
    chunks: Vec<Usage>,
}

impl Collected {
    fn push(&mut self, item: GenItem) {
        match item {
            GenItem::TextDelta { text } => self.text.push_str(&text),
            GenItem::Usage(usage) => {
                add(&mut self.usage, &usage);
                self.chunks.push(usage);
            }
            GenItem::Stop(stop) => self.stop = Some(stop),
            // Без тулов и истории thinking и картинки модулю не нужны.
            GenItem::ThinkingDelta { .. }
            | GenItem::ThinkingBlock { .. }
            | GenItem::RedactedThinking { .. }
            | GenItem::ToolUse { .. }
            | GenItem::Image { .. }
            | GenItem::Unknown => {}
        }
    }
}

fn add(sum: &mut Usage, chunk: &Usage) {
    sum.input += chunk.input;
    sum.cache_read += chunk.cache_read;
    sum.cache_write_short += chunk.cache_write_short;
    sum.cache_write_long += chunk.cache_write_long;
    sum.output += chunk.output;
    sum.reasoning += chunk.reasoning;
    sum.context_edits.applied += chunk.context_edits.applied;
    sum.context_edits.cleared_input_tokens += chunk.context_edits.cleared_input_tokens;
    sum.cost = match (sum.cost, chunk.cost) {
        (Some(a), Some(b)) => Some(a + b),
        (a, b) => a.or(b),
    };
    if chunk.rate_limits.is_some() {
        sum.rate_limits = chunk.rate_limits;
    }
}

/// Строки `spend`; сбой учёта не отнимает у модуля уже оплаченный ответ.
async fn record<C: Components>(
    state: &mut State<C>,
    entry: &ModelEntry,
    module: Option<&ModuleId>,
    chunks: &[Usage],
    failed: Option<ProviderErrorKind>,
) {
    if chunks.is_empty() && failed.is_none() {
        return;
    }
    let ts = state.world.now();
    let zone = state.shared.day_zone();
    let currency = state.shared.cfg().llm.currency.clone();
    let costs: Vec<Option<Cost>> = chunks.iter().map(|u| cost(entry, u, &currency)).collect();
    let model = entry.decl.name.clone();
    let rows: Vec<ModuleSpend> = chunks
        .iter()
        .zip(&costs)
        .map(|(u, cost)| ModuleSpend {
            ts,
            model: model.clone(),
            module: module.cloned(),
            input: u.input,
            cache_read: u.cache_read,
            cache_write: u.cache_write_short.saturating_add(u.cache_write_long),
            cache_write_long: u.cache_write_long,
            output: u.output,
            reasoning: u.reasoning,
            context_edits_applied: u.context_edits.applied,
            cost: cost.clone(),
        })
        .collect();
    let unpriced = costs.iter().filter(|c| c.is_none()).count();

    let written = state
        .shared
        .db()
        .write(move |tx| {
            let stats = tx.stats();
            for row in &rows {
                tx.modules().insert_spend(row)?;
            }
            for _ in 0..unpriced {
                stats.bump(Metric::SpendUnpriced, model.as_str(), ts, &zone)?;
            }
            match failed {
                Some(kind) => stats.bump(Metric::InferenceErrors, &kind.to_string(), ts, &zone),
                None => Ok(()),
            }
        })
        .await;
    if let Err(e) = written {
        log::error!(module:? = module, error:% = e; "failed to record inference spend");
    }
}

/// Прайс-бук `09 §2.1`: цена провайдера, иначе `ModelDecl.price`, иначе нет.
fn cost(entry: &ModelEntry, usage: &Usage, currency: &str) -> Option<Cost> {
    let (amount, source) = match (usage.cost, entry.decl.price) {
        (Some(cost), _) => (cost, CostSource::Provider),
        (None, Some(p)) => {
            let tokens = |n: u64, price: f64| n as f64 * price;
            let total = tokens(usage.input, p.input)
                + tokens(usage.cache_read, p.cache_read)
                + tokens(usage.cache_write_short, p.cache_write_short)
                + tokens(usage.cache_write_long, p.cache_write_long)
                + tokens(usage.output, p.output);
            (total / 1e6, CostSource::Pricebook)
        }
        (None, None) => return None,
    };
    Some(Cost {
        amount,
        currency: currency.to_owned(),
        source,
    })
}

/// Ответ по схеме; модель без нативной формы иногда оборачивает его в ограду.
fn parse_json(text: &str) -> Option<Value> {
    let text = text.trim();
    let bare = text
        .strip_prefix("```json")
        .or_else(|| text.strip_prefix("```"))
        .and_then(|t| t.strip_suffix("```"))
        .unwrap_or(text);
    serde_json::from_str(bare.trim()).ok()
}
