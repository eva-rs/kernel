//! Сценарный провайдер для тестов и смоука без ключей: отдаёт заготовленные
//! `GenItem`, пишет каждый запрос и отвергает `invalid`-ом то, на что живой
//! вендор ответил бы 400, — баги рендера ядра валят любой тест через мок.

mod echo;
mod script;
mod validate;

use std::sync::{
    Arc, Mutex, MutexGuard, PoisonError,
    atomic::{AtomicBool, Ordering},
};

use eva::data;
use tokio::sync::watch;

use eva_core::{GenSink, Provider};
use eva_protocol::types::{
    Effort, ModelId,
    generate::{
        Block, EmbedRequest, EmbedResponse, GenerateRequest, Message, ProviderError,
        ProviderErrorKind, ResultBlock, Usage,
    },
    model::{EmbedDecl, ModelCaps, ModelDecl, ProviderDecl},
};

pub use echo::sample_from_schema;
pub use script::{Gate, Script};

/// Чат-модель конфига по умолчанию.
pub const CHAT_MODEL: &str = "mock";
/// Эмбеддер конфига по умолчанию.
pub const EMBED_MODEL: &str = "mock-embed";

/// `llm.providers.<name>` с `kind: mock`.
#[data]
#[serde(default)]
pub struct Config {
    /// Пустой список — провайдер без моделей (I8).
    pub models: Vec<ModelDecl>,
    pub decl: ProviderDecl,
    pub on_exhausted: Exhausted,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            models: default_models(),
            decl: ProviderDecl {
                local: true,
                count_tokens: true,
            },
            on_exhausted: Exhausted::Echo,
        }
    }
}

/// Ответ на запрос сверх сценариев.
#[data(copy)]
#[derive(Default)]
pub enum Exhausted {
    /// `[mock:<model>] echo: …` последней user-реплики либо образец по `format`.
    #[default]
    Echo,
    /// Ошибка `invalid`: лишний шаг тура не прячется за эхом.
    Fail,
}

fn default_models() -> Vec<ModelDecl> {
    vec![
        ModelDecl {
            name: ModelId::from(CHAT_MODEL),
            context_window: 200_000,
            max_output: 32_000,
            caps: ModelCaps {
                vision: true,
                thinking: true,
                effort_levels: vec![Effort::Low, Effort::Medium, Effort::High, Effort::Max],
                structured_output: true,
                ..ModelCaps::default()
            },
            price: None,
            embed: None,
        },
        ModelDecl {
            name: ModelId::from(EMBED_MODEL),
            context_window: 8_192,
            max_output: 0,
            caps: ModelCaps::default(),
            price: None,
            embed: Some(EmbedDecl {
                dims: 64,
                max_input_tokens: 8_192,
            }),
        },
    ]
}

/// Дешёвый клон: тест держит копию для проверок, ядро — свою.
#[derive(Clone)]
pub struct MockProvider(Arc<Inner>);

struct Inner {
    config: Config,
    scripts: Mutex<std::collections::VecDeque<Script>>,
    requests: Mutex<Vec<GenerateRequest>>,
    seen: watch::Sender<usize>,
    /// Модели сверх конфига, дочитанные `settle`.
    listed: Mutex<Vec<ModelDecl>>,
    /// Каталог ещё не прочитан: `settle` ждёт `list`.
    awaiting: AtomicBool,
    listing: watch::Sender<Option<Vec<ModelDecl>>>,
}

impl MockProvider {
    pub fn new(config: Config) -> Self {
        Self(Arc::new(Inner {
            config,
            scripts: Mutex::default(),
            requests: Mutex::default(),
            seen: watch::Sender::new(0),
            listed: Mutex::default(),
            awaiting: AtomicBool::new(false),
            listing: watch::Sender::new(None),
        }))
    }

    /// Каталог недоступен до [`Self::list`]: так стартует провайдер без сети.
    #[must_use]
    pub fn awaiting_listing(self) -> Self {
        self.0.awaiting.store(true, Ordering::Release);
        self
    }

    /// Каталог ответил: `settle` добавит `models` к моделям конфига.
    pub fn list(&self, models: Vec<ModelDecl>) {
        self.0.listing.send_replace(Some(models));
    }

    /// Дочитывает каталог, как `openai`: ждёт [`Self::list`], если провайдер
    /// стартовал без него; `true` — `models()` сменился.
    pub async fn settle(&self) -> bool {
        if !self.0.awaiting.load(Ordering::Acquire) {
            return false;
        }
        let mut listing = self.0.listing.subscribe();
        let Ok(models) = listing
            .wait_for(Option::is_some)
            .await
            .map(|listed| listed.clone().unwrap_or_default())
        else {
            return false;
        };
        *lock(&self.0.listed) = models;
        self.0.awaiting.store(false, Ordering::Release);
        true
    }

    /// Модели по умолчанию; запрос сверх сценариев — ошибка.
    pub fn strict() -> Self {
        Self::new(Config {
            on_exhausted: Exhausted::Fail,
            ..Config::default()
        })
    }

    /// Сценарии расходуются по одному на `generate`, в порядке добавления.
    pub fn push(&self, script: impl Into<Script>) -> &Self {
        lock(&self.0.scripts).push_back(script.into());
        self
    }

    pub fn scripts_left(&self) -> usize {
        lock(&self.0.scripts).len()
    }

    /// Все запросы `generate` по порядку, включая отвергнутые.
    pub fn requests(&self) -> Vec<GenerateRequest> {
        lock(&self.0.requests).clone()
    }

    pub fn last_request(&self) -> Option<GenerateRequest> {
        lock(&self.0.requests).last().cloned()
    }

    /// Счётчик принятых запросов: тест ждёт «провайдер уже спрошен», не гадая таймером.
    pub fn seen(&self) -> watch::Receiver<usize> {
        self.0.seen.subscribe()
    }

    fn model(&self, name: &ModelId) -> Result<ModelDecl, ProviderError> {
        self.models()
            .into_iter()
            .find(|m| m.name == *name)
            .ok_or_else(|| {
                error(
                    ProviderErrorKind::NotFound,
                    format!("model {name} is not declared"),
                )
            })
    }

    fn record(&self, req: &GenerateRequest) {
        let count = {
            let mut requests = lock(&self.0.requests);
            requests.push(req.clone());
            requests.len()
        };
        self.0.seen.send_replace(count);
    }

    fn next_script(&self, req: &GenerateRequest) -> Result<Script, ProviderError> {
        if let Some(script) = lock(&self.0.scripts).pop_front() {
            return Ok(script);
        }
        match self.0.config.on_exhausted {
            Exhausted::Echo => Ok(echo::script(req, count_tokens(req))),
            Exhausted::Fail => Err(error(
                ProviderErrorKind::Invalid,
                "mock ran out of scripts for this request",
            )),
        }
    }
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

impl Provider for MockProvider {
    fn decl(&self) -> ProviderDecl {
        self.0.config.decl
    }

    fn models(&self) -> Vec<ModelDecl> {
        let mut models = self.0.config.models.clone();
        models.extend(lock(&self.0.listed).iter().cloned());
        models
    }

    async fn generate(&self, req: GenerateRequest, mut sink: GenSink) -> Result<(), ProviderError> {
        self.record(&req);
        let model = self.model(&req.model)?;
        validate::request(&req, &model).map_err(|raw| error(ProviderErrorKind::Invalid, raw))?;

        self.next_script(&req)?.play(&mut sink).await
    }

    async fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse, ProviderError> {
        let Some(embed) = self.model(&req.model)?.embed else {
            return Err(error(
                ProviderErrorKind::Invalid,
                format!("model {} is not an embedding model", req.model),
            ));
        };
        let vectors = req
            .inputs
            .iter()
            .map(|input| echo::vector(input, embed.dims))
            .collect();
        let bytes = req.inputs.iter().map(String::len).sum();
        Ok(EmbedResponse {
            vectors,
            usage: Usage {
                input: tokens(bytes),
                ..Usage::default()
            },
        })
    }

    async fn count_tokens(&self, req: GenerateRequest) -> Result<u32, ProviderError> {
        self.model(&req.model)?;
        Ok(u32::try_from(count_tokens(&req)).unwrap_or(u32::MAX))
    }
}

/// Байты текста / 4 — та же мера, что стартовый ratio ядра; картинки не считаются.
pub fn count_tokens(req: &GenerateRequest) -> u64 {
    let mut bytes = req.system.iter().map(|s| s.text.len()).sum::<usize>();
    for tool in &req.tools {
        bytes += tool.name.len() + tool.description.len() + tool.input_schema.to_string().len();
    }
    for message in &req.messages {
        let (Message::User { blocks }
        | Message::Assistant { blocks }
        | Message::System { blocks, .. }) = message;
        for block in blocks {
            bytes += match block {
                Block::Text { text } => text.len(),
                Block::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
                Block::ToolResult { content, .. } => content
                    .iter()
                    .map(|c| match c {
                        ResultBlock::Text { text } => text.len(),
                        ResultBlock::Image { .. } => 0,
                    })
                    .sum(),
                Block::Thinking { text, .. } => text.as_ref().map_or(0, String::len),
                Block::Image { .. } | Block::RedactedThinking { .. } => 0,
            };
        }
    }
    tokens(bytes)
}

fn tokens(bytes: usize) -> u64 {
    u64::try_from(bytes.div_ceil(4)).unwrap_or(u64::MAX)
}

fn error(kind: ProviderErrorKind, raw: impl Into<String>) -> ProviderError {
    ProviderError {
        kind,
        http_status: None,
        raw: raw.into(),
        retry_after: None,
    }
}

/// Паника теста под замком не должна ронять остальные проверки.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
