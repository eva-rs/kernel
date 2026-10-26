//! Харнесс soul без транспорта (`11 §8`): `Soul<Test>` на временном
//! каталоге, мок-провайдер `eva-provider-mock`, руки `Hands::local` в
//! песочнице, отказы БД и рук по флагу; `Harness::listen` поднимает UDS
//! поверх того же soul. Каждый файл `tests/*.rs` берёт отсюда нужное.
//!
//! Модули: `start_with(with_modules)`; клиенты звонят по `Url` из
//! `h.listen()` с `token(name)`, сервисы слушают `h.bind_service(name)` до
//! `h.listen()` и отвечают soul через `answer`.

#![allow(dead_code)]

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use eva::{
    time::{Clock as _, Mock},
    url::Url,
    utils::SecretString,
};
use eva_core::{Caller, SessionMaker as _, service};
use eva_hands::{Entry, Error, Greeting, Hands, Limits, Output};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::Response,
    types::{
        ChatId, ModelId, ModuleId, SessionId, SurfaceId,
        model::{ModelCaps, ModelDecl, ProviderDecl},
        module::{ModuleKind, ServiceAddress},
    },
    wire::{
        Capabilities,
        handshake::{Hello, ProtoVersion, Welcome},
    },
};
use eva_providers::{
    AnyProvider, ProviderConfig,
    mock::{self, MockProvider},
};
use eva_rpc::{
    ClientOptions, ConnectError, ModuleHandler, Psk, RpcClient, ServiceListener,
    config::{Listen, ServiceListen, UdsListen},
};

use eva_soul::{
    Components, Config, Parts, Session, Soul,
    config::{
        ChatsConfig, DbConfig, HandsConfig, LlmConfig, MemoryConfig, ModulesConfig, NotesConfig,
        SystemConfig, WorkConfig,
        hands::{EndpointConfig, HandsLimits},
        llm::ProviderEntry,
        modules::{ServiceEntry, TokenEntry},
    },
    db::{Connection, Db, DbError, DbResult, Queries as _, Reader, Sqlite, Tx},
    hands::Machine,
    providers::SoulProvider,
    subsystems::modules::{
        self, Listening,
        live::{Entry as LiveEntry, Source},
    },
    world::testing,
};

pub const MODEL: &str = "mock-model";
/// Имя провайдера харнесса в `llm.providers`.
pub const PROVIDER: &str = "mock";
pub const ENDPOINT: &str = "local";
/// Сервисный модуль харнесса и его surface.
pub const SVC: &str = "svc";
/// Второй сервисный модуль: «чужой» для `SVC`.
pub const OTHER: &str = "other";
pub const CLIENT: &str = "phone";
/// Второй клиент хозяина: `local` обслуживают оба.
pub const CLIENT2: &str = "tui";
/// Сокет слушателя относительно `system.state_dir`.
pub const SOCKET: &str = "modules.sock";

pub struct Test;

impl Components for Test {
    type Demiurge = testing::Demiurge;
    type Db = FaultyDb;
    type Provider = SoulProvider;
    type Hands = CountingHands;
}

/// SQLite с отказом по флагу: проверка пути `Aux::Db`.
pub struct FaultyDb {
    inner: Sqlite,
    pub fail: Arc<AtomicBool>,
}

impl FaultyDb {
    pub async fn open(path: &Path) -> Self {
        let inner = Sqlite::open(path, std::time::Duration::from_secs(5), 2)
            .await
            .expect("open test database");
        Self {
            inner,
            fail: Arc::default(),
        }
    }

    fn check(&self) -> DbResult<()> {
        if self.fail.load(Ordering::Relaxed) {
            Err(DbError::Sqlite("injected failure".into()))
        } else {
            Ok(())
        }
    }
}

impl Db for FaultyDb {
    async fn reader(&self) -> DbResult<Reader> {
        self.check()?;
        self.inner.reader().await
    }

    async fn write<T, E, F>(&self, f: F) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<DbError> + Send + 'static,
        F: FnOnce(&Tx<'_>) -> Result<T, E> + Send + 'static,
    {
        self.check()?;
        self.inner.write(f).await
    }

    async fn exclusive<T, E, F>(&self, f: F) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<DbError> + Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, E> + Send + 'static,
    {
        self.check()?;
        self.inner.exclusive(f).await
    }
}

/// Руки с подсчётом вызовов и инъекцией `Error::Unavailable`.
pub struct CountingHands {
    inner: Hands,
    pub calls: Arc<AtomicUsize>,
    pub unavailable: Arc<AtomicBool>,
}

impl CountingHands {
    pub fn local(limits: Limits) -> Self {
        Self {
            inner: Hands::local(limits),
            calls: Arc::default(),
            unavailable: Arc::default(),
        }
    }

    fn enter(&self) -> Result<&Hands, Error> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.unavailable.load(Ordering::Relaxed) {
            Err(Error::Unavailable("injected: machine is off".into()))
        } else {
            Ok(&self.inner)
        }
    }
}

impl Machine for CountingHands {
    async fn greeting(&self) -> Result<Greeting, Error> {
        self.enter()?.greeting().await
    }

    async fn stat(&self, cwd: Option<&str>, path: &str) -> Result<Entry, Error> {
        self.enter()?.stat(cwd, path).await
    }

    async fn list(&self, cwd: Option<&str>, path: &str) -> Result<(Vec<Entry>, bool), Error> {
        self.enter()?.list(cwd, path).await
    }

    async fn read(
        &self,
        cwd: Option<&str>,
        path: &str,
        offset: u64,
        len: Option<u64>,
    ) -> Result<(u64, Vec<u8>), Error> {
        self.enter()?.read(cwd, path, offset, len).await
    }

    async fn write(
        &self,
        cwd: Option<&str>,
        path: &str,
        data: &[u8],
        create_dirs: bool,
        mode: Option<u32>,
    ) -> Result<u64, Error> {
        self.enter()?
            .write(cwd, path, data, create_dirs, mode)
            .await
    }

    async fn find(
        &self,
        cwd: Option<&str>,
        path: &str,
        pattern: &[u8],
        from: u64,
        limit: u64,
    ) -> Result<(u64, Vec<u64>, bool), Error> {
        self.enter()?.find(cwd, path, pattern, from, limit).await
    }

    async fn mkdir(&self, cwd: Option<&str>, path: &str, parents: bool) -> Result<(), Error> {
        self.enter()?.mkdir(cwd, path, parents).await
    }

    async fn remove(&self, cwd: Option<&str>, path: &str, recursive: bool) -> Result<(), Error> {
        self.enter()?.remove(cwd, path, recursive).await
    }

    async fn rename(&self, cwd: Option<&str>, from: &str, to: &str) -> Result<(), Error> {
        self.enter()?.rename(cwd, from, to).await
    }

    async fn exec(
        &self,
        cwd: Option<&str>,
        command: &str,
        timeout_ms: u64,
        stdin: Vec<u8>,
    ) -> Result<Output, Error> {
        self.enter()?.exec(cwd, command, timeout_ms, stdin).await
    }
}

/// Модель мока харнесса: окно и потолок вывода — те, что проверяют тесты.
pub fn model_decl() -> ModelDecl {
    ModelDecl {
        name: ModelId::from(MODEL),
        context_window: 200_000,
        max_output: 8_192,
        caps: ModelCaps::default(),
        price: None,
        embed: None,
    }
}

/// Конфиг мока харнесса: одна модель `MODEL`, запрос сверх сценариев — эхо.
pub fn mock_config() -> mock::Config {
    mock::Config {
        models: vec![model_decl()],
        decl: ProviderDecl::default(),
        on_exhausted: mock::Exhausted::Echo,
    }
}

/// Токен клиента харнесса в `modules.tokens` (`with_modules`).
pub fn token(module: &str) -> String {
    format!("{module}-token")
}

/// Ключ сервиса харнесса в `modules.services` (`with_modules`): не короче 32 символов.
pub fn key(module: &str) -> String {
    format!("{module}-key-0123456789abcdef0123456789abcdef")
}

/// Сокет, по которому soul дозванивается до сервиса харнесса.
pub fn service_socket(dir: &Path, module: &str) -> PathBuf {
    dir.join(format!("{module}.sock"))
}

pub fn service_address(dir: &Path, module: &str) -> ServiceAddress {
    ServiceAddress::Uds(service_socket(dir, module))
}

/// Клиенты `CLIENT`, `CLIENT2` в `modules.tokens`, сервисы `SVC`, `OTHER` в
/// `modules.services` на сокетах `system.state_dir`.
pub fn with_modules(cfg: &mut Config) {
    for module in [CLIENT, CLIENT2] {
        cfg.modules.tokens.insert(
            ModuleId::from(module),
            TokenEntry::new(SecretString::Plaintext(token(module))),
        );
    }
    for module in [SVC, OTHER] {
        with_service(cfg, module);
    }
}

/// Сервис `module` в `modules.services` на сокете `system.state_dir`.
pub fn with_service(cfg: &mut Config, module: &str) {
    let address = service_address(&cfg.system.state_dir, module);
    cfg.modules.services.insert(
        ModuleId::from(module),
        ServiceEntry {
            address,
            key: SecretString::Plaintext(key(module)),
        },
    );
}

/// `hello` сервиса: без токена, идентичность — запись реестра.
pub fn dialed_hello(capabilities: Capabilities) -> Hello {
    Hello {
        version: String::from("1.2.3"),
        proto_version: ProtoVersion::CURRENT,
        token: None,
        capabilities,
        resume: None,
    }
}

/// Слушатель сервиса по `key(name)` на `path`.
pub async fn bind_service_at(path: &Path, name: &str) -> ServiceListener {
    bind_keyed(
        &ServiceListen::Uds(UdsListen {
            path: path.to_path_buf(),
        }),
        &key(name),
    )
    .await
}

/// Секция `module` сервиса на SDK: слушает там, куда soul его набирает (`with_service`).
pub fn sdk_service(dir: &Path, name: &str) -> eva_sdk::ServiceConfig {
    sdk_service_at(
        ServiceListen::Uds(UdsListen {
            path: service_socket(dir, name),
        }),
        name,
    )
}

/// Секция `module` сервиса `name` с ключом `key(name)` на своём слушателе.
pub fn sdk_service_at(listen: ServiceListen, name: &str) -> eva_sdk::ServiceConfig {
    eva_sdk::ServiceConfig {
        listen,
        key: SecretString::Plaintext(key(name)),
        soul_wait: eva::time::HumanDuration(DIAL_WAIT),
        transport: eva_rpc::Config::default(),
    }
}

/// Слушатель сервиса с секретом `key`: uds или tcp.
pub async fn bind_keyed(listen: &ServiceListen, key: &str) -> ServiceListener {
    ServiceListener::bind(listen, Psk::derive(key), &eva_rpc::Config::default())
        .await
        .expect("service binds")
}

/// Следующий дозвон soul: Noise responder и `hello` без токена.
pub async fn answer<H: ModuleHandler>(
    listener: &ServiceListener,
    hello: Hello,
    handler: H,
) -> Result<(RpcClient, Welcome), ConnectError> {
    let incoming = tokio::time::timeout(DIAL_WAIT, listener.accept())
        .await
        .expect("the soul dials in time")
        .expect("accept");
    let conn = incoming.secure().await.expect("noise handshake");
    RpcClient::answer(conn, hello, ClientOptions::default(), handler).await
}

/// Сессия сервиса по следующему дозвону soul: `capabilities` и `handler`.
pub async fn dialed<H: ModuleHandler>(
    listener: &ServiceListener,
    capabilities: Capabilities,
    handler: H,
) -> (RpcClient, Welcome) {
    answer(listener, dialed_hello(capabilities), handler)
        .await
        .expect("the soul dials the module")
}

/// Сколько `answer` ждёт дозвона: повтор после обрыва — через `[0.5s, 1s]`.
pub const DIAL_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Конфиг теста: всё в `dir`, один мок-провайдер, руки `local` в `dir/work`,
/// слушатель UDS `dir/modules.sock` без группы.
pub fn config(dir: &Path) -> Config {
    std::fs::write(
        dir.join("persona.md"),
        "You are Eva, a helpful personal assistant.",
    )
    .expect("test persona");
    let providers = BTreeMap::from([(
        String::from(PROVIDER),
        ProviderEntry::Builtin(ProviderConfig::Mock(mock_config())),
    )]);
    let limits = HandsLimits {
        roots: vec![dir.join("work")],
        ..HandsLimits::default()
    };

    Config {
        system: SystemConfig {
            state_dir: dir.to_path_buf(),
            tz: None,
        },
        db: DbConfig {
            path: Some(dir.join("eva.sqlite")),
            ..DbConfig::default()
        },
        chats: ChatsConfig::default(),
        context: Default::default(),
        modules: ModulesConfig {
            listen: vec![Listen::Uds(UdsListen {
                path: PathBuf::from(SOCKET),
            })],
            ..ModulesConfig::default()
        },
        llm: LlmConfig::new(providers, ModelId::from(MODEL)),
        hands: HandsConfig {
            endpoints: BTreeMap::from([(
                String::from(ENDPOINT),
                EndpointConfig::Local {
                    local: true,
                    limits,
                },
            )]),
            default_endpoint: Some(String::from(ENDPOINT)),
        },
        tools: Default::default(),
        work: WorkConfig::default(),
        notes: NotesConfig::default(),
        secrets: Default::default(),
        memory: MemoryConfig::default(),
        cron: Default::default(),
        sleep: Default::default(),
        gc: Default::default(),
        stats: Default::default(),
    }
}

pub struct Harness {
    pub soul: Soul<Test>,
    pub dir: tempfile::TempDir,
    /// Время всех миров soul: тест двигает его сам.
    pub clock: Mock,
    pub db_fail: Arc<AtomicBool>,
    pub hands_calls: Arc<AtomicUsize>,
    pub hands_unavailable: Arc<AtomicBool>,
    /// Клон провайдера soul: сценарии (`push`) и записанные запросы.
    pub provider: MockProvider,
}

/// Soul на свежем временном каталоге.
pub async fn start() -> Harness {
    start_with(|_| {}).await
}

pub async fn start_with(tweak: impl FnOnce(&mut Config)) -> Harness {
    start_with_mock(tweak, mock_config()).await
}

/// Soul с заданным конфигом мока: свои модели, цены, `on_exhausted`.
pub async fn start_with_mock(tweak: impl FnOnce(&mut Config), mock: mock::Config) -> Harness {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut cfg = config(dir.path());
    cfg.llm.providers.insert(
        String::from(PROVIDER),
        ProviderEntry::Builtin(ProviderConfig::Mock(mock.clone())),
    );
    tweak(&mut cfg);
    start_in_with(dir, cfg, MockProvider::new(mock)).await
}

/// Soul с готовым провайдером: например, стартовавшим без каталога моделей.
pub async fn start_with_provider(provider: MockProvider) -> Harness {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = config(dir.path());
    start_in_with(dir, cfg, provider).await
}

/// Soul поверх существующего каталога (рестарт на той же базе).
pub async fn start_in(dir: tempfile::TempDir, cfg: Config) -> Harness {
    start_in_with(dir, cfg, MockProvider::new(mock_config())).await
}

async fn start_in_with(dir: tempfile::TempDir, cfg: Config, provider: MockProvider) -> Harness {
    start_in_with_extra(dir, cfg, provider, BTreeMap::new()).await
}

/// Soul с провайдерами сверх мока харнесса; их записи в `llm.providers` кладёт `tweak`.
pub async fn start_with_providers(
    tweak: impl FnOnce(&mut Config),
    extra: BTreeMap<String, SoulProvider>,
) -> Harness {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut cfg = config(dir.path());
    tweak(&mut cfg);
    start_in_with_extra(dir, cfg, MockProvider::new(mock_config()), extra).await
}

async fn start_in_with_extra(
    dir: tempfile::TempDir,
    cfg: Config,
    provider: MockProvider,
    extra: BTreeMap<String, SoulProvider>,
) -> Harness {
    std::fs::create_dir_all(dir.path().join("work")).expect("work dir");

    let demiurge = testing::Demiurge::new(7);
    let clock = demiurge.clock.clone();
    let db = FaultyDb::open(&cfg.db_path()).await;
    let db_fail = db.fail.clone();

    let hands = CountingHands::local(Limits {
        roots: vec![dir.path().join("work")],
        ..Limits::default()
    });
    let (hands_calls, hands_unavailable) = (hands.calls.clone(), hands.unavailable.clone());

    let soul = Soul::start(
        cfg,
        Parts {
            demiurge,
            db,
            providers: BTreeMap::from([(
                String::from(PROVIDER),
                SoulProvider::Builtin(AnyProvider::Mock(provider.clone())),
            )])
            .into_iter()
            .chain(extra)
            .collect(),
            hands: BTreeMap::from([(String::from(ENDPOINT), hands)]),
        },
    )
    .await
    .expect("soul starts");

    Harness {
        soul,
        dir,
        clock,
        db_fail,
        hands_calls,
        hands_unavailable,
        provider,
    }
}

pub type TestSession = service::Session<Session<Test>>;

pub fn client(name: &str) -> Caller {
    Caller::Client {
        module: ModuleId::from(name),
    }
}

pub fn service(name: &str) -> Caller {
    Caller::Service {
        module: ModuleId::from(name),
    }
}

impl Harness {
    pub async fn session(&self, caller: Caller) -> TestSession {
        self.soul.make_session(caller).await.expect("make session")
    }

    pub async fn soul_session(&self) -> TestSession {
        self.session(Caller::Soul).await
    }

    pub async fn client_session(&self) -> TestSession {
        self.session(client(CLIENT)).await
    }

    /// Строка `modules`, как после handshake: на неё ссылается `surfaces.module`.
    pub async fn register_module(&self, name: &str) {
        let name = ModuleId::from(name);
        let now = self.clock.get();
        self.soul
            .shared()
            .db()
            .write(move |tx| tx.modules().connected(&name, "0.1.0", "0.1", &[0x80], now))
            .await
            .expect("register module");
    }

    /// Сервисный модуль `name` с собственным surface `name`.
    pub async fn service_with_surface(&self, name: &str) -> TestSession {
        self.register_module(name).await;
        let mut session = self.session(service(name)).await;
        session
            .surfaces()
            .create(SurfaceId::from(name), format!("{name} surface"), None)
            .await
            .expect("service creates its surface");
        session
    }

    /// Живая сессия модуля без транспорта, как после handshake: по ней soul
    /// выводит, кто обслуживает surface. Прежняя сессия модуля вытесняется.
    pub fn live_session(&self, module: &str, kind: ModuleKind, ask: bool) -> SessionId {
        let session = SessionId::from(format!("{module}-session"));
        let source = match kind {
            ModuleKind::Client => Source::ConfigToken,
            ModuleKind::Service => Source::ConfigService,
        };
        let entry = LiveEntry::new(
            session.clone(),
            Caller::from_kind(kind, ModuleId::from(module)),
            source,
            Capabilities {
                ask,
                ..Capabilities::default()
            },
            self.clock.get(),
        );
        _ = self
            .soul
            .shared()
            .live()
            .claim(ModuleId::from(module), entry, None)
            .expect("config sessions ignore revocations");
        session
    }

    /// Сессия модуля закрылась.
    pub fn drop_session(&self, module: &str, session: &SessionId) {
        self.soul
            .shared()
            .live()
            .release(&ModuleId::from(module), session)
            .expect("the session is current");
    }

    /// `ask_ui` чатов surface по живым сессиям.
    pub async fn ask_ui(&self, surface: &str) -> bool {
        let id = SurfaceId::from(surface);
        let row = self
            .query(move |c| c.surfaces().get(&id))
            .await
            .expect("surface exists");
        self.soul.shared().live().ask_ui(&row.surface_ref())
    }

    /// Модули, чьи живые сессии обслуживают surface.
    pub async fn serving(&self, surface: &str) -> Vec<String> {
        let id = SurfaceId::from(surface);
        let row = self
            .query(move |c| c.surfaces().get(&id))
            .await
            .expect("surface exists");
        self.soul
            .shared()
            .live()
            .serving(&row.surface_ref())
            .into_iter()
            .map(|m| m.as_str().to_owned())
            .collect()
    }

    /// Открытый сегмент чата: без него сбросу нечего закрывать.
    pub async fn open_segment(&self, chat: ChatId) -> i64 {
        self.soul
            .shared()
            .db()
            .write(move |tx| {
                Ok::<i64, DbError>(tx.query_row(
                    "INSERT INTO segments (chat_id, seq, started_at, reason, system_snapshot, tools_snapshot, hot_memory_ids)
                     VALUES (?1, 1, 1, 'initial', x'', x'', x'') RETURNING id",
                    [chat.0],
                    |r| r.get(0),
                )?)
            })
            .await
            .expect("segment")
    }

    /// Помечен ли открытый сегмент чата к сбросу (`05 §2.3`).
    pub async fn reset_pending(&self, chat: ChatId) -> bool {
        self.query(move |c| {
            Ok(c.query_row(
                "SELECT reset_pending IS NOT NULL FROM chats WHERE id = ?1",
                [chat.0],
                |r| r.get(0),
            )?)
        })
        .await
    }

    /// Сумма счётчика `health_stats` за все дни и ключи.
    pub async fn counter(&self, metric: &'static str) -> i64 {
        self.query(move |c| {
            Ok(c.query_row(
                "SELECT COALESCE(SUM(count), 0) FROM health_stats WHERE metric = ?1",
                [metric],
                |r| r.get(0),
            )?)
        })
        .await
    }

    /// Запрос к базе в обход операций: проверки состояния.
    pub async fn query<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> DbResult<T> + Send + 'static,
    ) -> T {
        let reader = self.soul.shared().db().reader().await.expect("reader");
        reader.run(f).await.expect("query")
    }
}

/// Исход операции для матриц `Policy` (`11 §8`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Forbidden,
    NotFound,
    Invalid,
    Conflict,
    Unavailable,
    /// Своя ошибка операции (`Spec`).
    Spec,
    /// Отказ SQLite: край различает его и `Internal` (`11 §7`).
    Db,
    /// `Internal`, `Quota`.
    Other,
}

pub fn outcome<O, E>(res: &Response<O, E>) -> Outcome {
    match res {
        Ok(_) => Outcome::Ok,
        Err(Generic::Spec(_)) => Outcome::Spec,
        Err(Generic::Aux(aux)) => match aux {
            Aux::Forbidden(_) => Outcome::Forbidden,
            Aux::NotFound(_) => Outcome::NotFound,
            Aux::Invalid(_) => Outcome::Invalid,
            Aux::Conflict(_) => Outcome::Conflict,
            Aux::Unavailable(_) => Outcome::Unavailable,
            Aux::Db(_) => Outcome::Db,
            Aux::Internal(_) | Aux::Quota(_) => Outcome::Other,
        },
    }
}

/// Caller'ы матрицы: soul, клиент, сервис своего surface A (`SVC`) и чужого B (`OTHER`).
pub fn matrix_callers() -> [(&'static str, Caller); 4] {
    [
        ("soul", Caller::Soul),
        ("client", client(CLIENT)),
        ("service-own", service(SVC)),
        ("service-foreign", service(OTHER)),
    ]
}

impl Harness {
    /// Слушатель сервиса `name` там, куда soul его набирает (`with_service`):
    /// поднятый до `listen`, он получит первую же попытку.
    pub async fn bind_service(&self, name: &str) -> ServiceListener {
        bind_service_at(&service_socket(self.dir.path(), name), name).await
    }

    /// Компонент `modules` поверх этого soul — слушатели и петли дозвона — и
    /// адрес UDS для клиентов; модули — `with_modules` в конфиге старта.
    pub async fn listen(&self) -> (Listening<Test>, Url) {
        let listening = modules::listen(&self.soul).await.expect("listen");
        let url = Url::parse(&format!("uds://{}", self.dir.path().join(SOCKET).display()))
            .expect("socket url");
        (listening, url)
    }
}
