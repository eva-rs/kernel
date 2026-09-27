//! `inference.complete` (`06 §5`, `09 §2`, `11 §6`): провайдер со сценарием
//! вместо общего мока — нужны текст, usage и ошибки.

mod common;

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use common::{CountingHands, FaultyDb, Outcome, SVC, outcome, service};
use eva_core::{
    ByteStream, CallStep as _, Caller, GenSink, Provider, SessionMaker as _, UploadStep as _,
    service::{self, blobs::Blobs as _, inference::Inference as _},
};
use eva_hands::Limits;
use eva_protocol::{
    errors::{Aux, Generic},
    requests::{blobs::put, inference::complete},
    types::{
        BlobId, ChatId, Effort, ModelId, ModuleId,
        blob::Kind,
        content::Block,
        generate::{
            self, CacheMode, EmbedRequest, EmbedResponse, GenItem, GenerateRequest, Message,
            ProviderError, ProviderErrorKind, Purpose, Stop, StopReason, Usage,
        },
        model::{ModelCaps, ModelDecl, Price, ProviderDecl},
    },
};
use eva_rpc::ModuleProvider;
use serde_json::json;

use eva_kernel::{
    Components, Kernel, Parts, Session,
    db::{Db as _, DbResult},
    providers::Settle,
    world::testing,
};

const CHEAP: &str = "cheap";
const PRICEY: &str = "pricey";
const VISION: &str = "vision";
const STRICT: &str = "strict";
/// Окно меньше `max_output` провайдера: без бюджета запрос переполнял бы его.
const NARROW: &str = "narrow";

#[derive(Clone, Default)]
struct Script {
    items: Vec<GenItem>,
    error: Option<ProviderError>,
}

#[derive(Clone, Default)]
struct Scripted {
    script: Arc<Mutex<Script>>,
    requests: Arc<Mutex<Vec<GenerateRequest>>>,
}

fn model(name: &str, price: Option<f64>, caps: ModelCaps) -> ModelDecl {
    ModelDecl {
        name: ModelId::from(name),
        context_window: 100_000,
        max_output: 4_096,
        caps,
        price: price.map(|input| Price {
            input,
            output: 2.0 * input,
            cache_read: input / 10.0,
            cache_write_short: 1.25 * input,
            cache_write_long: 2.0 * input,
        }),
        embed: None,
    }
}

impl Provider for Scripted {
    fn decl(&self) -> ProviderDecl {
        ProviderDecl::default()
    }

    fn models(&self) -> Vec<ModelDecl> {
        vec![
            model(PRICEY, Some(10.0), ModelCaps::default()),
            model(CHEAP, Some(1.0), ModelCaps::default()),
            model(
                VISION,
                None,
                ModelCaps {
                    vision: true,
                    ..ModelCaps::default()
                },
            ),
            model(
                STRICT,
                None,
                ModelCaps {
                    structured_output: true,
                    ..ModelCaps::default()
                },
            ),
            ModelDecl {
                context_window: 4_000,
                max_output: 4_000,
                ..model(NARROW, None, ModelCaps::default())
            },
        ]
    }

    async fn generate(&self, req: GenerateRequest, mut sink: GenSink) -> Result<(), ProviderError> {
        self.requests.lock().expect("requests").push(req);
        let script = self.script.lock().expect("script").clone();
        for item in script.items {
            if sink.push(item).await.is_err() {
                break;
            }
        }
        script.error.map_or(Ok(()), Err)
    }

    async fn embed(&self, _req: EmbedRequest) -> Result<EmbedResponse, ProviderError> {
        unimplemented!("not used by inference.complete")
    }

    async fn count_tokens(&self, _req: GenerateRequest) -> Result<u32, ProviderError> {
        unimplemented!("not used by inference.complete")
    }
}

/// Сценарий повторяется на каждый вызов и запросов не проверяет — ровно то,
/// что нужно этим тестам; модулей-провайдеров тут нет.
impl From<ModuleProvider> for Scripted {
    fn from(_: ModuleProvider) -> Self {
        unreachable!("inference tests connect no provider modules")
    }
}

#[derive(Debug, Clone)]
struct Spend {
    model: String,
    source: String,
    op: String,
    module: Option<String>,
    input: i64,
    cache_write: i64,
    cache_write_long: i64,
    cost: Option<f64>,
    cost_source: String,
}

/// Каталог моделей готов со старта.
impl Settle for Scripted {
    async fn settle(&self) -> bool {
        false
    }
}

struct Inf;

impl Components for Inf {
    type Demiurge = testing::Demiurge;
    type Db = FaultyDb;
    type Provider = Scripted;
    type Hands = CountingHands;
}

struct H {
    kernel: Kernel<Inf>,
    provider: Scripted,
    _dir: tempfile::TempDir,
}

type Sess = service::Session<Session<Inf>>;

async fn start(tweak: impl FnOnce(&mut eva_kernel::Config)) -> H {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(dir.path().join("work")).expect("work dir");
    let mut cfg = common::config(dir.path());
    cfg.llm.default_model = ModelId::from(PRICEY);
    tweak(&mut cfg);
    let provider = Scripted::default();

    let kernel = Kernel::start(
        cfg.clone(),
        Parts {
            demiurge: testing::Demiurge::new(7),
            db: FaultyDb::open(&cfg.db_path()).await,
            providers: BTreeMap::from([(String::from("mock"), provider.clone())]),
            hands: BTreeMap::from([(
                String::from(common::ENDPOINT),
                CountingHands::local(Limits {
                    roots: vec![dir.path().join("work")],
                    ..Limits::default()
                }),
            )]),
        },
    )
    .await
    .expect("kernel starts");

    H {
        kernel,
        provider,
        _dir: dir,
    }
}

impl H {
    async fn session(&self, caller: Caller) -> Sess {
        self.kernel.make_session(caller).await.expect("session")
    }

    fn script(&self, items: Vec<GenItem>, error: Option<ProviderError>) {
        *self.provider.script.lock().expect("script") = Script { items, error };
    }

    fn last_request(&self) -> GenerateRequest {
        self.provider
            .requests
            .lock()
            .expect("requests")
            .last()
            .cloned()
            .expect("a generate request")
    }

    async fn spend(&self) -> Vec<Spend> {
        let reader = self.kernel.shared().db().reader().await.expect("reader");
        reader
            .run(|c| -> DbResult<_> {
                let mut stmt = c.prepare(
                    "SELECT model, source, op, module, input, cache_write, cache_write_long, cost, cost_source
                     FROM spend ORDER BY id",
                )?;
                let rows = stmt
                    .query_map([], |r| {
                        Ok(Spend {
                            model: r.get(0)?,
                            source: r.get(1)?,
                            op: r.get(2)?,
                            module: r.get(3)?,
                            input: r.get(4)?,
                            cache_write: r.get(5)?,
                            cache_write_long: r.get(6)?,
                            cost: r.get(7)?,
                            cost_source: r.get(8)?,
                        })
                    })?
                    .collect::<Result<_, _>>()?;
                Ok(rows)
            })
            .await
            .expect("spend rows")
    }

    async fn register(&self, name: &str) {
        let name = ModuleId::from(name);
        self.kernel
            .shared()
            .db()
            .write(move |tx| {
                use eva_kernel::db::Queries as _;
                tx.modules().connected(
                    &name,
                    "0.1.0",
                    "0.1",
                    &[0x80],
                    eva::time::Timestamp::from_secs(1),
                )
            })
            .await
            .expect("register module");
    }

    async fn chat(&self, surface: &str) -> ChatId {
        let surface = surface.to_owned();
        self.kernel
            .shared()
            .db()
            .write(move |tx| -> DbResult<ChatId> {
                Ok(ChatId(tx.query_row(
                    "INSERT INTO chats (folder_id, kind, created_at, last_activity_at)
                     SELECT id, 'persistent', 1, 1 FROM folders WHERE surface_id = ?1 AND name = 'Unsorted'
                     RETURNING id",
                    [surface],
                    |r| r.get(0),
                )?))
            })
            .await
            .expect("insert chat")
    }
}

fn text(t: &str) -> GenItem {
    GenItem::TextDelta { text: t.to_owned() }
}

fn stop() -> GenItem {
    GenItem::Stop(Stop {
        reason: StopReason::EndTurn,
        details: None,
    })
}

fn usage(input: u64, output: u64, cost: Option<f64>) -> GenItem {
    GenItem::Usage(Usage {
        input,
        output,
        cache_write_short: 3,
        cache_write_long: 2,
        cost,
        ..Usage::default()
    })
}

fn ask(model: Option<&str>) -> complete::Args {
    complete::Args {
        model: model.map(ModelId::from),
        system: String::from("Classify."),
        input: vec![Block::text("hello")],
        schema: None,
        max_tokens: None,
        effort: None,
    }
}

#[tokio::test]
async fn complete_collects_text_usage_and_writes_spend() {
    let h = start(|_| {}).await;
    let mut svc = h.session(service(SVC)).await;
    h.script(
        vec![
            text("Hel"),
            usage(100, 0, None),
            text("lo"),
            usage(0, 20, Some(0.5)),
            stop(),
        ],
        None,
    );

    let ok = svc
        .inference()
        .complete()
        .call(ask(Some(CHEAP)))
        .await
        .expect("complete");
    assert_eq!(ok.text.as_deref(), Some("Hello"));
    assert_eq!(ok.json, None);
    assert_eq!(ok.stop.reason, StopReason::EndTurn);
    assert_eq!((ok.usage.input, ok.usage.output), (100, 20));
    assert_eq!(ok.usage.cache_write_short, 6);
    assert_eq!(ok.usage.cost, Some(0.5));

    let req = h.last_request();
    assert_eq!(req.model.as_str(), CHEAP);
    assert_eq!(req.cache, CacheMode::Off);
    assert_eq!(req.effort, Some(Effort::Low), "effort defaults to low");
    assert_eq!(req.thinking, None);
    assert_eq!(req.purpose, Purpose::Complete);
    assert_eq!(req.max_tokens, 4_096, "max_output of the model");
    assert!(req.tools.is_empty() && req.breakpoints.is_empty() && req.format.is_none());
    assert_eq!(req.system.len(), 1);
    assert_eq!(req.system[0].text, "Classify.");
    assert_eq!(
        req.messages,
        [Message::User {
            blocks: vec![generate::Block::Text {
                text: String::from("hello")
            }]
        }]
    );

    let pricebook = (100.0 * 1.0 + 3.0 * 1.25 + 2.0 * 2.0) / 1e6;
    let rows = h.spend().await;
    assert_eq!(rows.len(), 2, "a row per usage chunk: {rows:?}");
    let Spend {
        model,
        source,
        op,
        module,
        input,
        cache_write,
        cache_write_long,
        cost,
        cost_source,
    } = rows[0].clone();
    assert_eq!(
        (
            model.as_str(),
            source.as_str(),
            op.as_str(),
            module.as_deref()
        ),
        (CHEAP, "module", "other", Some(SVC))
    );
    assert_eq!((input, cache_write, cache_write_long), (100, 5, 2));
    assert!(
        (cost.expect("priced") - pricebook).abs() < 1e-12,
        "{cost:?}"
    );
    assert_eq!(cost_source, "pricebook");
    assert_eq!(
        (rows[1].cost, rows[1].cost_source.as_str()),
        (Some(0.5), "provider")
    );
}

#[tokio::test]
async fn model_defaults_to_the_cheapest_or_the_cheap_role() {
    let h = start(|_| {}).await;
    let mut client = h.session(common::client(common::CLIENT)).await;
    h.script(vec![stop()], None);

    client
        .inference()
        .complete()
        .call(ask(None))
        .await
        .expect("complete");
    assert_eq!(
        h.last_request().model.as_str(),
        CHEAP,
        "cheapest price.input"
    );

    let h = start(|cfg| cfg.llm.roles.cheap = Some(ModelId::from(VISION))).await;
    let mut client = h.session(common::client(common::CLIENT)).await;
    h.script(vec![stop()], None);
    client
        .inference()
        .complete()
        .call(ask(None))
        .await
        .expect("complete");
    assert_eq!(
        h.last_request().model.as_str(),
        VISION,
        "llm.roles.cheap wins"
    );
}

#[tokio::test]
async fn arguments_are_validated() {
    let h = start(|_| {}).await;
    let mut svc = h.session(service(SVC)).await;
    h.script(vec![stop()], None);

    let res = svc.inference().complete().call(ask(Some("nope"))).await;
    assert!(
        matches!(&res, Err(Generic::Aux(Aux::NotFound(what))) if what.contains("nope")),
        "{res:?}"
    );
    let res = svc
        .inference()
        .complete()
        .call(complete::Args {
            max_tokens: Some(0),
            ..ask(Some(CHEAP))
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Invalid);
    let res = svc
        .inference()
        .complete()
        .call(complete::Args {
            input: Vec::new(),
            ..ask(Some(CHEAP))
        })
        .await;
    assert_eq!(outcome(&res), Outcome::Invalid);

    svc.inference()
        .complete()
        .call(complete::Args {
            max_tokens: Some(1_000_000),
            effort: Some(Effort::High),
            ..ask(Some(CHEAP))
        })
        .await
        .expect("complete");
    let req = h.last_request();
    assert_eq!(req.max_tokens, 4_096, "clamped to max_output");
    assert_eq!(req.effort, Some(Effort::High));
    assert!(h.spend().await.is_empty(), "no usage — no spend rows");
}

#[tokio::test]
async fn provider_error_is_spec_and_usage_is_still_recorded() {
    let h = start(|_| {}).await;
    let mut svc = h.session(service(SVC)).await;
    h.script(
        vec![usage(7, 0, None)],
        Some(ProviderError {
            kind: ProviderErrorKind::Quota,
            http_status: Some(429),
            raw: String::from("slow down"),
            retry_after: None,
        }),
    );

    let res = svc.inference().complete().call(ask(Some(CHEAP))).await;
    assert!(
        matches!(&res, Err(Generic::Spec(complete::Err::Provider(e))) if e.error.kind == ProviderErrorKind::Quota),
        "{res:?}"
    );
    assert_eq!(h.spend().await.len(), 1);

    h.script(vec![text("cut")], None);
    let ok = svc
        .inference()
        .complete()
        .call(ask(Some(CHEAP)))
        .await
        .expect("stream without stop");
    assert_eq!(ok.stop.reason, StopReason::Other);
}

#[tokio::test]
async fn schema_goes_native_or_as_text() {
    let h = start(|_| {}).await;
    let mut svc = h.session(service(SVC)).await;
    let schema = json!({"type": "object", "properties": {"label": {"type": "string"}}});

    h.script(
        vec![text("```json\n{\"label\": \"spam\"}\n```"), stop()],
        None,
    );
    let ok = svc
        .inference()
        .complete()
        .call(complete::Args {
            schema: Some(schema.clone()),
            ..ask(Some(CHEAP))
        })
        .await
        .expect("fallback");
    assert_eq!(ok.json, Some(json!({"label": "spam"})));
    let req = h.last_request();
    assert!(req.format.is_none());
    assert!(
        req.system.iter().any(|s| s.text.contains("\"label\"")),
        "schema is in the prompt: {:?}",
        req.system
    );

    h.script(vec![text("{\"label\": \"ham\"}"), stop()], None);
    let ok = svc
        .inference()
        .complete()
        .call(complete::Args {
            schema: Some(schema.clone()),
            ..ask(Some(STRICT))
        })
        .await
        .expect("native");
    assert_eq!(ok.json, Some(json!({"label": "ham"})));
    let format = h.last_request().format.expect("native structured output");
    assert_eq!(format.schema, schema);
    assert!(format.strict);

    h.script(vec![text("not json"), stop()], None);
    let ok = svc
        .inference()
        .complete()
        .call(complete::Args {
            schema: Some(schema),
            ..ask(Some(CHEAP))
        })
        .await
        .expect("unparsable");
    assert_eq!((ok.text.as_deref(), ok.json), (Some("not json"), None));
}

async fn put_blob(session: &mut Sess, chat: ChatId, name: &str, kind: Kind) -> BlobId {
    let body: ByteStream = Box::pin(futures::stream::iter([Ok(eva::bytes::Bytes::from_static(
        b"\x89PNG",
    ))]));
    session
        .blobs()
        .put()
        .call(
            put::Args {
                chat,
                name: name.to_owned(),
                media_type: String::from("image/png"),
                kind,
                size: 4,
                hash: None,
            },
            body,
        )
        .await
        .expect("blobs.put")
        .id
}

#[tokio::test]
async fn blob_blocks_follow_vision_and_policy() {
    let h = start(|_| {}).await;
    h.register(SVC).await;
    let mut svc = h.session(service(SVC)).await;
    create_surface(&mut svc).await;
    let own = h.chat(SVC).await;
    let local = h.chat("local").await;
    let img = put_blob(&mut svc, own, "cat.png", Kind::Image).await;
    let doc = put_blob(&mut svc, own, "notes.bin", Kind::File).await;
    let mut kernel = h.session(Caller::Kernel).await;
    let foreign = put_blob(&mut kernel, local, "secret.png", Kind::Image).await;
    h.script(vec![stop()], None);

    let input = vec![
        Block::Image {
            blob: img.clone(),
            media_type: String::from("image/png"),
        },
        Block::File { blob: doc.clone() },
    ];
    svc.inference()
        .complete()
        .call(complete::Args {
            input: input.clone(),
            ..ask(Some(VISION))
        })
        .await
        .expect("vision");
    let Message::User { blocks } = &h.last_request().messages[0] else {
        panic!("user message");
    };
    assert_eq!(
        blocks[0],
        generate::Block::Image {
            media_type: String::from("image/png"),
            data: b"\x89PNG".to_vec().into(),
        }
    );
    assert_eq!(
        blocks[1],
        generate::Block::Text {
            text: format!("[file {doc}] notes.bin (4 bytes)")
        }
    );

    svc.inference()
        .complete()
        .call(complete::Args {
            input,
            ..ask(Some(CHEAP))
        })
        .await
        .expect("no vision");
    let Message::User { blocks } = &h.last_request().messages[0] else {
        panic!("user message");
    };
    assert_eq!(
        blocks[0],
        generate::Block::Text {
            text: format!("[image {img}] cat.png")
        }
    );

    let res = svc
        .inference()
        .complete()
        .call(complete::Args {
            input: vec![Block::File { blob: foreign }],
            ..ask(Some(CHEAP))
        })
        .await;
    assert_eq!(
        outcome(&res),
        Outcome::Forbidden,
        "blob of a foreign chat: {res:?}"
    );
    let res = svc
        .inference()
        .complete()
        .call(complete::Args {
            input: vec![Block::File {
                blob: BlobId::from("missing"),
            }],
            ..ask(Some(CHEAP))
        })
        .await;
    assert_eq!(outcome(&res), Outcome::NotFound, "{res:?}");
}

#[tokio::test]
async fn policy_matrix() {
    let h = start(|_| {}).await;
    h.script(vec![usage(1, 1, None), stop()], None);

    for (who, caller) in common::matrix_callers() {
        let mut session = h.session(caller).await;
        let res = session.inference().complete().call(ask(Some(CHEAP))).await;
        assert_eq!(outcome(&res), Outcome::Ok, "{who}: {res:?}");
    }
    let spend = h.spend().await;
    let modules: Vec<Option<String>> = spend
        .iter()
        .filter(|r| r.source == "module")
        .map(|r| r.module.clone())
        .collect();
    assert_eq!(
        modules,
        [
            Some(String::from(common::CLIENT)),
            Some(String::from(SVC)),
            Some(String::from(common::OTHER))
        ],
        "spend is charged to the calling module"
    );
    assert!(
        spend
            .iter()
            .all(|r| (r.source == "module") == r.module.is_some()),
        "the kernel is never charged as a module: {spend:?}"
    );
    let system: Vec<_> = spend.iter().filter(|r| r.source == "system").collect();
    assert_eq!(
        system.len(),
        1,
        "kernel inference is charged to system: {spend:?}"
    );
    assert_eq!((system[0].op.as_str(), &system[0].module), ("other", &None));
}

/// Вход и `max_tokens` вместе не превышают окна: иначе провайдер отвечает 400.
#[tokio::test]
async fn max_tokens_fits_the_context_window() {
    let h = start(|_| {}).await;
    h.script(vec![stop()], None);
    let mut svc = h.session(service(SVC)).await;
    // `Classify.` + `hello` = 14 байт → 4 токена по ratio 4.
    let small = 4;
    let room = 4_000 - small - 1_024;

    let call = |input: String, max_tokens| complete::Args {
        input: vec![Block::text(input)],
        max_tokens,
        ..ask(Some(NARROW))
    };
    for (requested, want) in [(None, room), (Some(3_500), room), (Some(100), 100)] {
        svc.inference()
            .complete()
            .call(call(String::from("hello"), requested))
            .await
            .expect("complete");
        assert_eq!(h.last_request().max_tokens, want, "requested {requested:?}");
    }

    // 11 991 байт входа ≈ 3 000 токенов: на вывод остаётся меньше margin.
    let res = svc
        .inference()
        .complete()
        .call(call("x".repeat(11_991), None))
        .await;
    assert!(
        matches!(&res, Err(Generic::Aux(Aux::Invalid(why))) if why.contains("does not fit")),
        "{res:?}"
    );

    // Калиброванный ratio модели меняет оценку: 2 000 байт — 1 000 токенов, не 500.
    let input = "y".repeat(1_991);
    svc.inference()
        .complete()
        .call(call(input.clone(), None))
        .await
        .expect("default ratio");
    assert_eq!(h.last_request().max_tokens, 4_000 - 500 - 1_024);
    h.kernel
        .shared()
        .db()
        .write(|tx| {
            tx.execute(
                "INSERT INTO token_ratios (model, bytes_per_token, samples, updated_at) VALUES (?1, 2.0, 10, 1)",
                [NARROW],
            )?;
            Ok::<_, eva_kernel::db::DbError>(())
        })
        .await
        .expect("calibrate");
    svc.inference()
        .complete()
        .call(call(input, None))
        .await
        .expect("calibrated ratio");
    assert_eq!(h.last_request().max_tokens, 4_000 - 1_000 - 1_024);
}

async fn create_surface(session: &mut Sess) {
    use eva_core::service::surfaces::Surfaces as _;
    use eva_protocol::{requests::surfaces::create, types::SurfaceId};

    session
        .surfaces()
        .create()
        .call(create::Args {
            id: SurfaceId::from(SVC),
            name: String::from("svc"),
            rules: None,
        })
        .await
        .expect("surfaces.create");
}
