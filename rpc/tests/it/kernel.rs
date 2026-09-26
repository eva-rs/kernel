//! Ядро для тестов транспорта: несколько операций по-настоящему, остальное —
//! заглушки `stub!` (`11 §8`).

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

use eva::time::Timestamp;
use eva_core::{
    ByteStream, CallStep, Caller, Items, Session, SessionMaker, StreamStep, UploadStep,
    service::{
        AuxFut, RespFut, agents::Agents, blobs::Blobs, chats::Chats, cron::Cron, folders::Folders,
        fs::Fs, hands_grants::HandsGrants, inference::Inference, kv::Kv, memory::Memory,
        models::Models, modules::Modules, notes::Notes, persons::Persons, sleep::Sleep,
        sqlite::Sqlite, stats::Stats, surfaces::Surfaces, tools::Tools, turns::Turns,
    },
};
use eva_protocol::{
    errors::{self, Aux, Generic},
    requests::{
        self, Response,
        blobs::put,
        surfaces::{create, get, list},
        turns::attach,
    },
    types::{
        BlobId, ChatId, EventId, SessionId, TurnId,
        module::TokenKind,
        surface::Surface,
        turn::{FeedEvent, TurnItem},
    },
    wire::{
        ErrorCode, RpcError,
        handshake::{Hello, ProtoVersion, Welcome},
    },
};
use eva_rpc::Authenticator;

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

/// Шаг из замыкания.
pub struct Call<F>(F);

impl<A, O, E, F, Fut> CallStep<A> for Call<F>
where
    F: FnMut(A) -> Fut + Send + Sync,
    Fut: Future<Output = Response<O, E>> + Send,
{
    type Ok = O;
    type Err = E;

    fn call(&mut self, args: A) -> impl RespFut<O, E> {
        (self.0)(args)
    }
}

pub struct Open<F>(F);

impl<A, T, E, F, Fut> StreamStep<A> for Open<F>
where
    F: FnMut(A) -> Fut + Send + Sync,
    Fut: Future<Output = Response<Items<T, E>, E>> + Send,
{
    type Item = T;
    type Err = E;

    fn open(&mut self, args: A) -> impl RespFut<Items<T, E>, E> {
        (self.0)(args)
    }
}

pub struct Upload<F>(F);

impl<A, O, E, F, Fut> UploadStep<A> for Upload<F>
where
    F: FnMut(A, ByteStream) -> Fut + Send + Sync,
    Fut: Future<Output = Response<O, E>> + Send,
{
    type Ok = O;
    type Err = E;

    fn call(&mut self, args: A, body: ByteStream) -> impl RespFut<O, E> {
        (self.0)(args, body)
    }
}

fn stubbed<O, E>() -> Response<O, E> {
    unimplemented!("operation is stubbed in this test kernel")
}

pub struct Fail<A, O, E>(PhantomData<fn(A) -> (O, E)>);

pub fn fail<A, O, E>() -> Fail<A, O, E> {
    Fail(PhantomData)
}

impl<A, O: Send, E: Send> CallStep<A> for Fail<A, O, E> {
    type Ok = O;
    type Err = E;

    fn call(&mut self, _: A) -> impl RespFut<O, E> {
        async { stubbed() }
    }
}

impl<A, T: Send, E: Send> StreamStep<A> for Fail<A, T, E> {
    type Item = T;
    type Err = E;

    fn open(&mut self, _: A) -> impl RespFut<Items<T, E>, E> {
        async { stubbed() }
    }
}

macro_rules! stub {
    ($($m:ident => $($seg:ident)::+),* $(,)?) => {$(
        fn $m(&mut self) -> impl CallStep<$($seg)::+::Args, Ok = $($seg)::+::Ok, Err = $($seg)::+::Err> {
            fail()
        }
    )*};
}

impl Surfaces for TestSession {
    fn create(&mut self) -> impl CallStep<create::Args, Ok = create::Ok, Err = create::Err> {
        let state = self.state.clone();
        Call(move |args: create::Args| {
            let state = state.clone();
            async move {
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
        })
    }

    fn get(&mut self) -> impl CallStep<get::Args, Ok = get::Ok, Err = get::Err> {
        let state = self.state.clone();
        Call(move |args: get::Args| {
            let found = state
                .surfaces
                .lock()
                .unwrap()
                .iter()
                .find(|s| s.id == args.surface)
                .cloned();
            async move {
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
        })
    }

    fn list(&mut self) -> impl CallStep<list::Args, Ok = list::Ok, Err = list::Err> {
        Call(|_: list::Args| async { Err(Generic::Aux(Aux::forbidden("owner only"))) })
    }

    stub! { update => requests::surfaces::update }
}

impl Turns for TestSession {
    fn attach(&mut self) -> impl StreamStep<attach::Args, Item = attach::Item, Err = attach::Err> {
        let state = self.state.clone();
        Open(move |args: attach::Args| {
            let state = state.clone();
            async move {
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
                    chat::BROKEN => {
                        Box::pin(stream::iter([Ok(item(0)), Ok(item(1)), Err(not_found())]))
                    }
                    _ => return Err(not_found()),
                };
                Ok(items)
            }
        })
    }

    stub! {
        start => requests::turns::start,
        append_silent => requests::turns::append_silent,
        delivered => requests::turns::delivered,
        cancel => requests::turns::cancel,
        respond => requests::turns::respond,
    }
}

struct DropFlag(Arc<State>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.attach_dropped.notify_one();
    }
}

impl Blobs for TestSession {
    fn put(&mut self) -> impl UploadStep<put::Args, Ok = put::Ok, Err = put::Err> {
        let state = self.state.clone();
        Upload(move |args: put::Args, mut body: ByteStream| {
            let state = state.clone();
            async move {
                let mut bytes = Vec::new();
                while let Some(chunk) = body.next().await {
                    bytes.extend_from_slice(&chunk.map_err(Generic::Aux)?);
                }
                let actual = bytes.len() as u64;
                if actual != args.size {
                    return Err(Generic::Spec(
                        errors::blobs::SizeMismatch {
                            declared: args.size,
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
        })
    }

    fn get(
        &mut self,
    ) -> impl StreamStep<
        requests::blobs::get::Args,
        Item = requests::blobs::get::Item,
        Err = requests::blobs::get::Err,
    > {
        fail()
    }

    stub! { stat => requests::blobs::stat }
}

impl Folders for TestSession {
    stub! {
        create => requests::folders::create,
        update => requests::folders::update,
        delete => requests::folders::delete,
        list => requests::folders::list,
    }
}

impl Chats for TestSession {
    stub! {
        ingest => requests::chats::ingest,
        create => requests::chats::create,
        get => requests::chats::get,
        list => requests::chats::list,
        update => requests::chats::update,
        r#move => requests::chats::r#move,
        delete => requests::chats::delete,
        restore => requests::chats::restore,
        search => requests::chats::search,
        history_read => requests::chats::history_read,
        compact => requests::chats::compact,
        notify => requests::chats::notify,
    }
}

impl Persons for TestSession {
    stub! {
        resolve => requests::persons::resolve,
        list => requests::persons::list,
        rename => requests::persons::rename,
        merge => requests::persons::merge,
        whitelist => requests::persons::whitelist,
    }
}

impl Memory for TestSession {
    stub! {
        list => requests::memory::list,
        get => requests::memory::get,
        history => requests::memory::history,
        restore => requests::memory::restore,
    }
}

impl Notes for TestSession {
    stub! { list => requests::notes::list }
}

impl Tools for TestSession {
    stub! { catalog => requests::tools::catalog }
}

impl Models for TestSession {
    stub! { list => requests::models::list }
}

impl Cron for TestSession {
    stub! { list => requests::cron::list, runs => requests::cron::runs }
}

impl Sleep for TestSession {
    stub! { runs => requests::sleep::runs, phase_runs => requests::sleep::phase_runs }
}

impl Agents for TestSession {
    stub! { list => requests::agents::list }
}

impl Stats for TestSession {
    stub! {
        tools_usage => requests::stats::tools_usage,
        spend => requests::stats::spend,
        spend_series => requests::stats::spend_series,
        chat_spend => requests::stats::chat_spend,
        health => requests::stats::health,
    }
}

impl Modules for TestSession {
    stub! {
        list => requests::modules::list,
        token_issue => requests::modules::token_issue,
        token_revoke => requests::modules::token_revoke,
    }
}

impl HandsGrants for TestSession {
    stub! { list => requests::hands_grants::list, revoke => requests::hands_grants::revoke }
}

impl Inference for TestSession {
    stub! { complete => requests::inference::complete }
}

impl Fs for TestSession {
    stub! {
        read => requests::fs::read,
        write => requests::fs::write,
        list => requests::fs::list,
        stat => requests::fs::stat,
        remove => requests::fs::remove,
    }
}

impl Sqlite for TestSession {
    stub! {
        exec => requests::sqlite::exec,
        query => requests::sqlite::query,
        tx => requests::sqlite::tx,
    }
}

impl Kv for TestSession {
    stub! {
        get => requests::kv::get,
        set => requests::kv::set,
        del => requests::kv::del,
        list => requests::kv::list,
    }
}

pub const CLIENT_TOKEN: &str = "client-token";
pub const SERVICE_TOKEN: &str = "service-token";

/// Токен → бит `kind`; зарезервированные имена и чужой токен — `unauthorized`.
#[derive(Default)]
pub struct Auth {
    sessions: AtomicU64,
}

impl Authenticator for Auth {
    async fn authenticate(&self, hello: &Hello) -> Result<Welcome, RpcError> {
        if matches!(hello.module.as_str(), "kernel" | "system" | "local") {
            return Err(RpcError::new(
                ErrorCode::Unauthorized,
                "reserved module name",
            ));
        }
        let kind = match hello.token.expose() {
            CLIENT_TOKEN => TokenKind::Client,
            SERVICE_TOKEN => TokenKind::Service,
            _ => return Err(RpcError::new(ErrorCode::Unauthorized, "unknown token")),
        };
        let n = self.sessions.fetch_add(1, Ordering::SeqCst);

        Ok(Welcome {
            session_id: SessionId(format!("s{n}")),
            kernel_version: "test".to_owned(),
            proto_version: ProtoVersion::CURRENT,
            kind,
            services: Vec::new(),
            tool_ids: Default::default(),
            models_accepted: Vec::new(),
            warnings: Vec::new(),
        })
    }
}

pub fn chat_id(n: i64) -> ChatId {
    ChatId(n)
}
