//! Харнесс ядра без транспорта (`11 §8`): `Kernel<Test>` на временном
//! каталоге, мок-провайдер `eva-provider-mock`, руки `Hands::local` в
//! песочнице, отказы БД и рук по флагу; `Harness::listen` поднимает UDS
//! поверх того же ядра. Каждый файл `tests/*.rs` берёт отсюда нужное.
//!
//! Модули по UDS: `start_with(with_module_tokens)`, затем `h.listen()` →
//! `(Listening, Url)`; токен модуля — `token(name)`.

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
use eva_core::{
    CallStep as _, Caller, SessionMaker as _, service, service::surfaces::Surfaces as _,
};
use eva_hands::{Entry, Error, Greeting, Hands, Limits, Output};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::{Response, surfaces},
    types::{
        ChatId, ModelId, ModuleId, SessionId, SurfaceId,
        model::{ModelCaps, ModelDecl, ProviderDecl},
        module::TokenKind,
    },
    wire::Capabilities,
};
use eva_providers::{
    AnyProvider, ProviderConfig,
    mock::{self, MockProvider},
};
use eva_rpc::config::{FileMode, Listen, UdsListen};

use eva_kernel::{
    Components, Config, Kernel, Parts, Session,
    config::{
        ChatsConfig, DbConfig, HandsConfig, LlmConfig, ModulesConfig, NotesConfig, SystemConfig,
        WorkConfig,
        hands::{EndpointConfig, HandsLimits},
        llm::ProviderEntry,
        modules::TokenEntry,
    },
    db::{Connection, Db, DbError, DbResult, Queries as _, Reader, Sqlite, Tx},
    hands::Machine,
    providers::KernelProvider,
    subsystems::modules::{
        self, Listening,
        live::{Entry as LiveEntry, TokenSource},
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
    type Provider = KernelProvider;
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

/// Токен модуля харнесса в `modules.tokens` (`with_module_tokens`).
pub fn token(module: &str) -> String {
    format!("{module}-token")
}

/// `modules.tokens`: клиенты `CLIENT`, `CLIENT2`, сервисы `SVC`, `OTHER`.
pub fn with_module_tokens(cfg: &mut Config) {
    let entries = [
        (CLIENT, TokenKind::Client),
        (CLIENT2, TokenKind::Client),
        (SVC, TokenKind::Service),
        (OTHER, TokenKind::Service),
    ];
    for (module, kind) in entries {
        cfg.modules.tokens.insert(
            ModuleId::from(module),
            TokenEntry {
                token: SecretString::Plaintext(token(module)),
                kind,
            },
        );
    }
}

/// Конфиг теста: всё в `dir`, один мок-провайдер, руки `local` в `dir/work`,
/// слушатель UDS `dir/modules.sock` без группы.
pub fn config(dir: &Path) -> Config {
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
        modules: ModulesConfig {
            listen: vec![Listen::Uds(UdsListen {
                path: PathBuf::from(SOCKET),
                mode: FileMode::default(),
                group: None,
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
        work: WorkConfig::default(),
        notes: NotesConfig::default(),
    }
}

pub struct Harness {
    pub kernel: Kernel<Test>,
    pub dir: tempfile::TempDir,
    /// Время всех миров ядра: тест двигает его сам.
    pub clock: Mock,
    pub db_fail: Arc<AtomicBool>,
    pub hands_calls: Arc<AtomicUsize>,
    pub hands_unavailable: Arc<AtomicBool>,
    /// Клон провайдера ядра: сценарии (`push`) и записанные запросы.
    pub provider: MockProvider,
}

/// Ядро на свежем временном каталоге.
pub async fn start() -> Harness {
    start_with(|_| {}).await
}

pub async fn start_with(tweak: impl FnOnce(&mut Config)) -> Harness {
    start_with_mock(tweak, mock_config()).await
}

/// Ядро с заданным конфигом мока: свои модели, цены, `on_exhausted`.
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

/// Ядро с готовым провайдером: например, стартовавшим без каталога моделей.
pub async fn start_with_provider(provider: MockProvider) -> Harness {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = config(dir.path());
    start_in_with(dir, cfg, provider).await
}

/// Ядро поверх существующего каталога (рестарт на той же базе).
pub async fn start_in(dir: tempfile::TempDir, cfg: Config) -> Harness {
    start_in_with(dir, cfg, MockProvider::new(mock_config())).await
}

async fn start_in_with(dir: tempfile::TempDir, cfg: Config, provider: MockProvider) -> Harness {
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

    let kernel = Kernel::start(
        cfg,
        Parts {
            demiurge,
            db,
            providers: BTreeMap::from([(
                String::from(PROVIDER),
                KernelProvider::Builtin(AnyProvider::Mock(provider.clone())),
            )]),
            hands: BTreeMap::from([(String::from(ENDPOINT), hands)]),
        },
    )
    .await
    .expect("kernel starts");

    Harness {
        kernel,
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
        self.kernel
            .make_session(caller)
            .await
            .expect("make session")
    }

    pub async fn kernel_session(&self) -> TestSession {
        self.session(Caller::Kernel).await
    }

    pub async fn client_session(&self) -> TestSession {
        self.session(client(CLIENT)).await
    }

    /// Строка `modules`, как после handshake: на неё ссылается `surfaces.module`.
    pub async fn register_module(&self, name: &str) {
        let name = ModuleId::from(name);
        let now = self.clock.get();
        self.kernel
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
            .create()
            .call(surfaces::create::Args {
                id: SurfaceId::from(name),
                name: format!("{name} surface"),
                rules: None,
            })
            .await
            .expect("service creates its surface");
        session
    }

    /// Живая сессия модуля без транспорта, как после handshake: по ней ядро
    /// выводит, кто обслуживает surface. Прежняя сессия модуля вытесняется.
    pub fn live_session(&self, module: &str, kind: TokenKind, ask: bool) -> SessionId {
        let session = SessionId::from(format!("{module}-session"));
        let entry = LiveEntry::new(
            session.clone(),
            Caller::from_token(kind, ModuleId::from(module)),
            TokenSource::Config,
            Capabilities {
                ask,
                ..Capabilities::default()
            },
            self.clock.get(),
        );
        _ = self
            .kernel
            .shared()
            .live()
            .claim(ModuleId::from(module), entry, None)
            .expect("config sessions ignore revocations");
        session
    }

    /// Сессия модуля закрылась.
    pub fn drop_session(&self, module: &str, session: &SessionId) {
        self.kernel
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
        self.kernel.shared().live().ask_ui(&row.surface_ref())
    }

    /// Модули, чьи живые сессии обслуживают surface.
    pub async fn serving(&self, surface: &str) -> Vec<String> {
        let id = SurfaceId::from(surface);
        let row = self
            .query(move |c| c.surfaces().get(&id))
            .await
            .expect("surface exists");
        self.kernel
            .shared()
            .live()
            .serving(&row.surface_ref())
            .into_iter()
            .map(|m| m.as_str().to_owned())
            .collect()
    }

    /// Открытый сегмент чата: без него сбросу нечего закрывать.
    pub async fn open_segment(&self, chat: ChatId) -> i64 {
        self.kernel
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
        let reader = self.kernel.shared().db().reader().await.expect("reader");
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

/// Caller'ы матрицы: ядро, клиент, сервис своего surface A (`SVC`) и чужого B (`OTHER`).
pub fn matrix_callers() -> [(&'static str, Caller); 4] {
    [
        ("kernel", Caller::Kernel),
        ("client", client(CLIENT)),
        ("service-own", service(SVC)),
        ("service-foreign", service(OTHER)),
    ]
}

impl Harness {
    /// Слушатели `modules.listen` поверх этого ядра и адрес UDS для модулей;
    /// токены модулей — `with_module_tokens` в конфиге старта.
    pub async fn listen(&self) -> (Listening<Test>, Url) {
        let listening = modules::listen(&self.kernel).await.expect("listen");
        let url = Url::parse(&format!("uds://{}", self.dir.path().join(SOCKET).display()))
            .expect("socket url");
        (listening, url)
    }
}
