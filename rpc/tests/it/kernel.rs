//! Ядро для тестов транспорта: несколько операций по-настоящему, остальное —
//! заглушки; реализации трейтов неймспейсов порождает `for_each_op!` (`11 §8`).

use std::{
    marker::PhantomData,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures::{StreamExt as _, stream};
use tokio::sync::Notify;

use eva::{fut::Fut, time::Timestamp};
use eva_core::{CallStep, Caller, Session, SessionMaker, service::AuxFut};
use eva_protocol::{
    errors::{self, Aux, Generic},
    requests::{
        Items, Response,
        blobs::put,
        surfaces::{create, get, list},
        turns::attach,
    },
    types::{
        BlobId, ChatId, EventId, ModuleId, SessionId, TurnId,
        module::ModuleKind,
        surface::Surface,
        turn::{FeedEvent, TurnItem},
    },
    wire::{
        ErrorCode, RpcError,
        handshake::{Hello, ProtoVersion, Token, Welcome},
    },
};
use eva_rpc::{Authenticator, Origin};

/// Чаты `turns.attach` с разным поведением потока.
pub mod chat {
    /// Три элемента и чистый конец.
    pub const FINITE: i64 = 1;
    /// Один элемент, дальше тишина; drop потока отмечается в `State::attach_dropped`.
    pub const ENDLESS: i64 = 2;
    /// Поток быстрее любого буфера; drop тоже отмечается.
    pub const FLOOD: i64 = 3;
    /// Два элемента, затем `Spec(NotFound)` последним элементом.
    pub const BROKEN: i64 = 5;
    /// Отказ до первого элемента.
    pub const MISSING: i64 = 404;
}

pub const FLOOD_ITEMS: i64 = 10_000;

#[derive(Default)]
pub struct State {
    pub surfaces: Mutex<Vec<Surface>>,
    /// Сколько раз `surfaces.create` исполнился по-настоящему.
    pub creates: AtomicUsize,
    /// `surfaces.create` ждёт столько перед ответом.
    pub create_delay: Mutex<Duration>,
    pub attach_dropped: Notify,
    pub uploads: Mutex<Vec<Vec<u8>>>,
    /// `blobs.put` ждёт столько, прежде чем читать тело.
    pub put_delay: Mutex<Duration>,
}

#[derive(Clone, Default)]
pub struct Kernel {
    pub state: Arc<State>,
}

pub struct TestSession {
    state: Arc<State>,
}

impl SessionMaker for Kernel {
    type Session = TestSession;

    fn make_session(&self, caller: Caller) -> impl AuxFut<Session<TestSession>> {
        let session = Session::new(
            TestSession {
                state: self.state.clone(),
            },
            caller,
        );
        async move { Ok(session) }
    }
}

pub fn surface(id: &str) -> Surface {
    Surface {
        id: id.into(),
        name: id.to_owned(),
        module: Some("mod".into()),
        rules: None,
        trusted: None,
        model: None,
        effort: None,
        thinking: None,
        compact_at: None,
        builtin: None,
        created_at: Timestamp::from_secs(1_750_000_000),
    }
}

pub fn item(n: i64) -> TurnItem {
    TurnItem {
        seq: EventId(n),
        sub: 0,
        replayed: false,
        event: FeedEvent::TurnError {
            turn: TurnId::from("t"),
            message: format!("item {n}"),
        },
    }
}

/// Шаг настоящей операции над общим `State`.
struct Real<F> {
    state: Arc<State>,
    op: F,
}

impl<A, O, F, R> CallStep<A> for Real<F>
where
    F: Fn(Arc<State>, A) -> R + Send + Sync,
    R: Fut<Output = O>,
{
    type Output = O;

    fn call(&mut self, args: A) -> impl Fut<Output = O> {
        (self.op)(self.state.clone(), args)
    }
}

struct Fail<O, E>(PhantomData<fn() -> (O, E)>);

impl<A: Send, O: Send, E: Send> CallStep<A> for Fail<O, E> {
    type Output = Response<O, E>;

    async fn call(&mut self, _: A) -> Self::Output {
        unimplemented!("operation is stubbed in this test kernel")
    }
}

/// Операции, которые тестовое ядро исполняет; остальные — `Fail`.
macro_rules! reply {
    (surfaces create $session:ident) => {
        real!($session surfaces_create)
    };
    (surfaces get $session:ident) => {
        real!($session surfaces_get)
    };
    (surfaces list $session:ident) => {
        real!($session surfaces_list)
    };
    (turns attach $session:ident) => {
        real!($session turns_attach)
    };
    (blobs put $session:ident) => {
        real!($session blobs_put)
    };
    ($segment:ident $endpoint:ident $session:ident) => {
        Fail(PhantomData)
    };
}

macro_rules! real {
    ($session:ident $op:ident) => {
        Real {
            state: $session.state.clone(),
            op: $op,
        }
    };
}

macro_rules! test_session {
    ($(
        $segment:ident {$(
            $accessor:ident : $Trait:ident {$(
                $(#[$flag:ident])*
                $method:ident = $endpoint:ident {
                    args: $Args:ty,
                    ok: $Ok:ty,
                    err: $Err:ty,
                    output: $Output:ty,
                    docs: [$($doc:literal),*],
                }
            )*}
        )*}
    )*) => {$($(
        impl eva_core::service::$segment::$Trait for TestSession {$(
            fn $method(&mut self) -> impl CallStep<$Args, Output = $Output> {
                reply!($segment $endpoint self)
            }
        )*}
    )*)*};
}

eva_core::for_each_op!(test_session);

async fn surfaces_create(
    state: Arc<State>,
    args: create::Args,
) -> Response<create::Ok, create::Err> {
    state.creates.fetch_add(1, Ordering::SeqCst);
    let delay = *state.create_delay.lock().unwrap();
    tokio::time::sleep(delay).await;

    let mut surfaces = state.surfaces.lock().unwrap();
    if surfaces.iter().any(|s| s.id == args.id) {
        let taken = errors::surfaces::Taken {
            surface: args.id,
            owner: Some("mod".into()),
        };
        return Err(Generic::Spec(taken.into()));
    }
    let surface = surface(args.id.as_str());
    surfaces.push(surface.clone());
    Ok(create::Ok { surface })
}

async fn surfaces_get(state: Arc<State>, args: get::Args) -> Response<get::Ok, get::Err> {
    let found = state
        .surfaces
        .lock()
        .unwrap()
        .iter()
        .find(|s| s.id == args.surface)
        .cloned();
    match found {
        Some(surface) => Ok(get::Ok { surface }),
        None => Err(Generic::Spec(
            errors::surfaces::NotFound {
                surface: args.surface,
            }
            .into(),
        )),
    }
}

async fn surfaces_list(_: Arc<State>, _: list::Args) -> Response<list::Ok, list::Err> {
    Err(Generic::Aux(Aux::forbidden("owner only")))
}

async fn turns_attach(state: Arc<State>, args: attach::Args) -> Response<attach::Ok, attach::Err> {
    let not_found = || {
        Generic::Spec(attach::Err::from(errors::chats::NotFound {
            chat: args.chat.into(),
        }))
    };
    let items: Items<TurnItem, attach::Err> = match args.chat.get() {
        chat::FINITE => Box::pin(stream::iter((0..3).map(|n| Ok(item(n))))),
        chat::ENDLESS => {
            let guard = DropFlag(state);
            Box::pin(
                stream::once(async { Ok(item(0)) })
                    .chain(stream::pending())
                    .map(move |item| {
                        let _ = &guard;
                        item
                    }),
            )
        }
        chat::FLOOD => {
            let guard = DropFlag(state);
            Box::pin(stream::iter(0..FLOOD_ITEMS).map(move |n| {
                let _ = &guard;
                Ok(item(n))
            }))
        }
        chat::BROKEN => Box::pin(stream::iter([Ok(item(0)), Ok(item(1)), Err(not_found())])),
        _ => return Err(not_found()),
    };
    Ok(items)
}

struct DropFlag(Arc<State>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.attach_dropped.notify_one();
    }
}

async fn blobs_put(
    state: Arc<State>,
    put::Args { meta, mut body }: put::Args,
) -> Response<put::Ok, put::Err> {
    let delay = *state.put_delay.lock().unwrap();
    tokio::time::sleep(delay).await;
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        bytes.extend_from_slice(&chunk.map_err(Generic::Aux)?);
    }
    let actual = bytes.len() as u64;
    if actual != meta.size {
        return Err(Generic::Spec(
            errors::blobs::SizeMismatch {
                declared: meta.size,
                actual,
            }
            .into(),
        ));
    }
    state.uploads.lock().unwrap().push(bytes);
    Ok(put::Ok {
        id: BlobId::from("b1"),
        size: actual,
        hash: format!("len:{actual}"),
    })
}

pub const CLIENT_TOKEN: &str = "client-token";
/// Второй клиент: чужой для `CLIENT_TOKEN`.
pub const OTHER_TOKEN: &str = "other-token";

/// Модуль токена `CLIENT_TOKEN`.
pub const CLIENT: &str = "mod";
/// Сервис, которого набирает ядро.
pub const SERVICE: &str = "bridge";
/// Модуль токена `OTHER_TOKEN`.
pub const OTHER: &str = "other";
/// Набранная запись, которой реестр уже не знает.
pub const REMOVED: &str = "removed";

/// Токен → клиент, набранная запись → сервис; чужое — `unauthorized`.
#[derive(Default)]
pub struct Auth {
    sessions: AtomicU64,
}

impl Authenticator for Auth {
    type Dialed = ModuleId;

    async fn authenticate(
        &self,
        hello: &Hello,
        origin: Origin<'_, ModuleId>,
    ) -> Result<Welcome, RpcError> {
        let (module, kind) = match origin {
            Origin::Dialed(module) if module.as_str() == REMOVED => {
                return Err(RpcError::new(ErrorCode::Unauthorized, "module removed"));
            }
            Origin::Dialed(module) => (module.clone(), ModuleKind::Service),
            Origin::Listener => match hello.token.as_ref().map(Token::expose) {
                Some(CLIENT_TOKEN) => (ModuleId::from(CLIENT), ModuleKind::Client),
                Some(OTHER_TOKEN) => (ModuleId::from(OTHER), ModuleKind::Client),
                _ => return Err(RpcError::new(ErrorCode::Unauthorized, "unknown token")),
            },
        };
        let n = self.sessions.fetch_add(1, Ordering::SeqCst);

        Ok(Welcome {
            session_id: SessionId(format!("s{n}")),
            module,
            kernel_version: "test".to_owned(),
            proto_version: ProtoVersion::CURRENT,
            kind,
            services: Vec::new(),
            tool_ids: Default::default(),
            models_accepted: Vec::new(),
            warnings: Vec::new(),
            resumed: false,
        })
    }
}

pub fn chat_id(n: i64) -> ChatId {
    ChatId(n)
}
