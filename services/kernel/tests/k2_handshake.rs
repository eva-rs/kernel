//! Гонки handshake (`06 §2`, `§7`, `§9`): что происходит между проверкой
//! токена, строкой `modules` и `welcome`. Ядро на БД с крючком: первая
//! запись после `arm` сначала исполняет крючок — так тест встаёт ровно
//! внутрь handshake.

mod common;

use std::{
    collections::BTreeMap,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use common::{CLIENT, ENDPOINT, FaultyDb, PROVIDER, SVC, client, service, token};
use eva::{time::Timestamp, url::Url};
use eva_core::{
    CallStep as _, Caller, SessionMaker as _,
    service::{chats::Chats as _, modules::Modules as _, surfaces::Surfaces as _},
};
use eva_hands::Limits;
use eva_kernel::{
    Components, Kernel, Parts,
    db::{Connection, Db, DbError, DbResult, Queries as _, Reader, Tx},
    providers::KernelProvider,
    subsystems::modules::{
        self, Listening,
        live::{Entry, Live, Refused, TokenSource},
    },
    world::testing,
};
use eva_protocol::{
    errors::Aux,
    requests::{chats, modules as ops, surfaces},
    types::{ModuleId, SessionId, SurfaceId, module::TokenKind},
    wire::{
        Capabilities, ErrorCode, RpcError,
        capabilities::EventsCap,
        events::{Event, Publish, Topic},
        handshake::{Hello, ProtoVersion},
    },
};
use eva_providers::{AnyProvider, mock::MockProvider};
use eva_rpc::{ClientOptions, ModuleHandler, NoHandler, RpcClient};
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(5);
const BRIDGE: &str = "bridge";

type Hook = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Крючок следующей записи: исполняется до неё, один раз.
#[derive(Clone, Default)]
struct Hooks(Arc<Mutex<Option<Hook>>>);

impl Hooks {
    fn arm(&self, hook: impl Future<Output = ()> + Send + 'static) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::pin(hook));
    }

    fn take(&self) -> Option<Hook> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

struct HookDb {
    inner: FaultyDb,
    hooks: Hooks,
    /// Крючок следующего чтения: проверка выданного токена в handshake.
    reads: Hooks,
}

impl Db for HookDb {
    async fn reader(&self) -> DbResult<Reader> {
        let hook = self.reads.take();
        if let Some(hook) = hook {
            hook.await;
        }
        self.inner.reader().await
    }

    async fn write<T, E, F>(&self, f: F) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<DbError> + Send + 'static,
        F: FnOnce(&Tx<'_>) -> Result<T, E> + Send + 'static,
    {
        let hook = self.hooks.take();
        if let Some(hook) = hook {
            hook.await;
        }
        self.inner.write(f).await
    }

    async fn exclusive<T, E, F>(&self, f: F) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<DbError> + Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, E> + Send + 'static,
    {
        self.inner.exclusive(f).await
    }
}

struct Hooked;

impl Components for Hooked {
    type Demiurge = testing::Demiurge;
    type Db = HookDb;
    type Provider = KernelProvider;
    type Hands = common::CountingHands;
}

struct Rig {
    kernel: Kernel<Hooked>,
    hooks: Hooks,
    reads: Hooks,
    url: Url,
    listening: Listening<Hooked>,
    _dir: tempfile::TempDir,
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::create_dir_all(dir.path().join("work")).expect("work dir");
    let mut cfg = common::config(dir.path());
    common::with_module_tokens(&mut cfg);

    let (hooks, reads) = (Hooks::default(), Hooks::default());
    let db = HookDb {
        inner: FaultyDb::open(&cfg.db_path()).await,
        hooks: hooks.clone(),
        reads: reads.clone(),
    };
    let hands = common::CountingHands::local(Limits {
        roots: vec![dir.path().join("work")],
        ..Limits::default()
    });
    let kernel = Kernel::start(
        cfg,
        Parts {
            demiurge: testing::Demiurge::new(7),
            db,
            providers: BTreeMap::from([(
                String::from(PROVIDER),
                KernelProvider::Builtin(AnyProvider::Mock(
                    MockProvider::new(common::mock_config()),
                )),
            )]),
            hands: BTreeMap::from([(String::from(ENDPOINT), hands)]),
        },
    )
    .await
    .expect("kernel starts");
    let listening = modules::listen(&kernel).await.expect("listen");
    let url = Url::parse(&format!(
        "uds://{}",
        dir.path().join(common::SOCKET).display()
    ))
    .expect("socket url");

    Rig {
        kernel,
        hooks,
        reads,
        url,
        listening,
        _dir: dir,
    }
}

fn hello(token: &str, capabilities: Capabilities) -> Hello {
    Hello {
        version: String::from("1.0.0"),
        proto_version: ProtoVersion::CURRENT,
        token: token.into(),
        capabilities,
        resume: None,
    }
}

async fn closed(client: &RpcClient) -> RpcError {
    tokio::time::timeout(WAIT, client.closed())
        .await
        .expect("connection closes")
}

/// Модуль, складывающий `events.publish` в канал.
#[derive(Clone)]
struct Events(mpsc::UnboundedSender<Publish>);

impl ModuleHandler for Events {
    async fn event(&self, event: Publish) {
        _ = self.0.send(event);
    }
}

#[tokio::test]
async fn events_published_during_the_handshake_reach_the_module() {
    let rig = rig().await;
    let module = ModuleId::from(SVC);
    let now = rig.kernel.shared().now();
    let name = module.clone();
    rig.kernel
        .shared()
        .db()
        .write(move |tx| tx.modules().connected(&name, "0.1.0", "0.1", &[0x80], now))
        .await
        .expect("module row");
    rig.kernel
        .make_session(service(SVC))
        .await
        .expect("session")
        .surfaces()
        .create()
        .call(surfaces::create::Args {
            id: SurfaceId::from(SVC),
            name: String::from("Service"),
            rules: None,
        })
        .await
        .expect("surface");

    // Чат появляется, пока ядро пишет строку `modules` этого hello.
    let kernel = rig.kernel.clone();
    rig.hooks.arm(async move {
        kernel
            .make_session(Caller::Kernel)
            .await
            .expect("kernel session")
            .chats()
            .create()
            .call(chats::create::Args {
                surface: SurfaceId::from(SVC),
                folder: None,
                external_id: None,
                external_kind: None,
                kind: None,
                title: None,
                project: None,
                workdir: None,
                workdir_on: None,
            })
            .await
            .expect("chat during the handshake");
    });

    let (tx, mut rx) = mpsc::unbounded_channel();
    let caps = Capabilities {
        events: Some(EventsCap {
            subscriptions: vec![Topic::ChatCreated],
        }),
        ..Capabilities::default()
    };
    let (_client, _) = RpcClient::connect_with(
        &rig.url,
        hello(&token(SVC), caps),
        ClientOptions::default(),
        Events(tx),
    )
    .await
    .expect("connects");

    let publish = tokio::time::timeout(WAIT, rx.recv())
        .await
        .expect("the event is not lost")
        .expect("channel open");
    assert!(
        matches!(&publish.event, Event::ChatCreated(chat) if chat.surface == SurfaceId::from(SVC)),
        "{publish:?}"
    );
}

#[tokio::test]
async fn a_session_revoked_before_welcome_registers_nothing() {
    let rig = rig().await;
    let issued = rig
        .kernel
        .make_session(client(CLIENT))
        .await
        .expect("owner")
        .modules()
        .token_issue()
        .call(ops::token_issue::Args {
            module: ModuleId::from(BRIDGE),
            kind: TokenKind::Service,
            issued_by: None,
        })
        .await
        .expect("token_issue")
        .token;
    let mut bus = rig.kernel.shared().bus().subscribe();

    // Токен отзывают, когда сессия уже заявлена, но `welcome` ещё не ушёл.
    let kernel = rig.kernel.clone();
    rig.hooks.arm(async move {
        kernel
            .make_session(client(CLIENT))
            .await
            .expect("owner")
            .modules()
            .token_revoke()
            .call(ops::token_revoke::Args {
                module: ModuleId::from(BRIDGE),
            })
            .await
            .expect("token_revoke");
    });

    let caps = Capabilities {
        events: Some(EventsCap {
            subscriptions: vec![Topic::ChatCreated],
        }),
        ..Capabilities::default()
    };
    let (bridge, _) = RpcClient::connect_with(
        &rig.url,
        hello(&issued, caps),
        ClientOptions::default(),
        NoHandler,
    )
    .await
    .expect("welcome was already on its way");
    assert_eq!(closed(&bridge).await.code, ErrorCode::Unauthorized);

    let disconnected = tokio::time::timeout(WAIT, async {
        loop {
            match bus.recv().await.expect("bus open").event {
                Event::ModuleConnected(e) if e.module.as_str() == BRIDGE => {
                    panic!("a revoked session announced itself")
                }
                Event::ModuleDisconnected(e) if e.module.as_str() == BRIDGE => break,
                _ => {}
            }
        }
    })
    .await;
    assert!(disconnected.is_ok(), "the revoked session is cleaned up");
}

#[tokio::test]
async fn requests_of_a_closed_session_are_not_served() {
    let h = common::start().await;
    let (live, module) = (h.kernel.shared().live(), ModuleId::from(BRIDGE));
    let entry = Entry::new(
        SessionId::from(String::from("s1")),
        service(BRIDGE),
        TokenSource::Issued,
        Capabilities::default(),
        Timestamp::from_secs(1),
    );
    let epoch = live.epoch(&module);
    assert!(matches!(
        live.claim(module.clone(), entry, Some(epoch)),
        Ok(None)
    ));
    assert!(h.kernel.make_session(service(BRIDGE)).await.is_ok());

    live.revoke(
        &module,
        RpcError::new(ErrorCode::Unauthorized, "module token revoked"),
    );
    let res = h.kernel.make_session(service(BRIDGE)).await;
    assert!(matches!(res.err(), Some(Aux::Unavailable(_))));
    assert!(
        h.kernel.make_session(service(SVC)).await.is_ok(),
        "other modules are untouched"
    );
}

fn entry(session: &str, source: TokenSource) -> Entry {
    Entry::new(
        SessionId::from(String::from(session)),
        service(BRIDGE),
        source,
        Capabilities::default(),
        Timestamp::from_secs(1),
    )
}

fn revoked() -> RpcError {
    RpcError::new(ErrorCode::Unauthorized, "module token revoked")
}

/// Отзыв между проверкой токена и регистрацией сессии: проверка устарела.
#[test]
fn a_revoke_between_token_check_and_claim_refuses_the_hello() {
    let (live, module) = (Live::default(), ModuleId::from(BRIDGE));
    let seen = live.epoch(&module);
    assert!(!live.revoke(&module, revoked()), "no session to close yet");

    let res = live.claim(module.clone(), entry("s1", TokenSource::Issued), Some(seen));
    assert!(matches!(res, Err(Refused::Revoked)), "{res:?}");
    assert!(live.is_empty());

    let fresh = live.epoch(&module);
    let res = live.claim(
        module.clone(),
        entry("s2", TokenSource::Issued),
        Some(fresh),
    );
    assert!(matches!(res, Ok(None)), "{res:?}");
    let res = live.claim(module, entry("s3", TokenSource::Config), None);
    assert!(
        matches!(res, Ok(Some(_))),
        "a config token ignores revocations"
    );
}

#[test]
fn a_revoked_session_stays_closed_until_released() {
    let (live, module) = (Live::default(), ModuleId::from(BRIDGE));
    let seen = live.epoch(&module);
    let res = live.claim(module.clone(), entry("s1", TokenSource::Issued), Some(seen));
    assert!(matches!(res, Ok(None)), "{res:?}");
    assert!(live.closed(&module).is_none());

    assert!(live.revoke(&module, revoked()));
    let reason = live.closed(&module).expect("its requests are refused");
    assert_eq!(reason.code, ErrorCode::Unauthorized);
    let session = SessionId::from(String::from("s1"));
    assert!(live.release(&module, &session).is_some());
    assert!(live.closed(&module).is_none());
}

/// `hello`, проверявший токен, пока остановка закрывала сессии, не заводит
/// сессию после `shut_down`: иначе она переживёт остановку.
#[tokio::test]
async fn a_handshake_finishing_after_shut_down_gets_no_session() {
    let Rig {
        kernel,
        reads,
        url,
        listening,
        _dir,
        ..
    } = rig().await;
    let issued = kernel
        .make_session(client(CLIENT))
        .await
        .expect("owner")
        .modules()
        .token_issue()
        .call(ops::token_issue::Args {
            module: ModuleId::from(BRIDGE),
            kind: TokenKind::Service,
            issued_by: None,
        })
        .await
        .expect("token_issue")
        .token;

    let (entered, inside) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    reads.arm(async move {
        _ = entered.send(());
        _ = released.await;
    });
    let hello = hello(&issued, Capabilities::default());
    let connecting = tokio::spawn(async move {
        RpcClient::connect_with(&url, hello, ClientOptions::default(), NoHandler).await
    });
    tokio::time::timeout(WAIT, inside)
        .await
        .expect("the handshake checks the token")
        .expect("hook ran");

    let stopping = tokio::spawn(async move {
        let started = tokio::time::Instant::now();
        listening.shutdown().await;
        started.elapsed()
    });
    // Остановка успевает закрыть сессии до того, как проверка токена вернётся.
    tokio::time::sleep(Duration::from_millis(200)).await;
    _ = release.send(());

    let took = tokio::time::timeout(Duration::from_secs(10), stopping)
        .await
        .expect("shutdown returns")
        .expect("shutdown task");
    assert!(
        took < Duration::from_secs(2),
        "shutdown waited {took:?} for a session opened after shut_down"
    );
    let res = connecting.await.expect("connect task");
    assert!(
        kernel.shared().live().is_empty(),
        "a session outlived the shutdown"
    );
    assert!(
        matches!(&res, Err(eva_rpc::ConnectError::Rejected(e)) if e.code == ErrorCode::Unavailable),
        "{res:?}"
    );
}
