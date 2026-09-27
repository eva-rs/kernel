//! `Kernel` модуля: каждый вызов берёт текущее соединение, поэтому ручка,
//! взятая до реконнекта, после него ведёт в новую сессию.
//!
//! Вызов на закрытом соединении при живом `run` ждёт реконнекта до
//! `CALL_RECONNECT_WAIT`. Оборванные в полёте чтение и открытие потока
//! повторяются на новом соединении, мутация — с тем же id запроса, только
//! если ядро продолжило окно идемпотентности (`welcome.resumed`, `06 §5`) и
//! не исполнит её второй раз; иначе (рестарт ядра) исход неизвестен, и она
//! отвечает `Aux::Unavailable`. Загрузка отвечает `Aux::Unavailable`
//! (`11 §6`): её тело уже прочитано.

use std::{
    marker::PhantomData,
    sync::{Arc, PoisonError, RwLock},
    time::Duration,
};

use futures::StreamExt as _;
use serde::{Serialize, de::DeserializeOwned};
use tokio::{sync::watch, time::Instant};

use eva_core::{ByteStream, CallStep, Items, Session, StreamStep, UploadStep};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::Response,
    wire::{ErrorCode, RpcError, Welcome, frame::Payload},
};
use eva_rpc::{Peer, RpcClient, RpcSession, client::from_rpc, codec};

use crate::Kernel;

/// Сессия одного конкретного соединения.
type Rpc = Session<RpcSession>;

/// Сколько вызов ждёт нового соединения; столько же ядро ждёт реконнекта
/// ради незавершённого `tools.call` (`modules.call_reconnect_grace`).
pub const CALL_RECONNECT_WAIT: Duration = Duration::from_secs(30);

/// Текущее соединение модуля; меняется на реконнекте.
pub(crate) struct Current {
    pub client: RpcClient,
    pub welcome: Welcome,
}

#[derive(Clone, Copy)]
struct Phase {
    /// Растёт с каждым установленным соединением.
    generation: u64,
    /// Растёт с каждым соединением, не продолжившим окно идемпотентности
    /// прошлого: id мутаций из старого окна новому ядру неизвестны.
    window: u64,
    /// `run` жив и умеет переподключаться: только тогда закрытое соединение стоит ждать.
    reconnects: bool,
}

/// Текущее соединение и его смены.
pub(crate) struct Link {
    current: RwLock<Option<Current>>,
    phase: watch::Sender<Phase>,
}

/// Снимок текущего соединения для одного вызова.
struct Live {
    session: Rpc,
    peer: Peer,
    generation: u64,
    window: u64,
}

impl Link {
    pub fn new() -> Self {
        Self {
            current: RwLock::new(None),
            phase: watch::Sender::new(Phase {
                generation: 0,
                window: 0,
                reconnects: false,
            }),
        }
    }

    /// Новое соединение — текущее; ждущие вызовы просыпаются.
    pub fn install(&self, current: Current) {
        let resumed = current.welcome.resumed;
        let previous = {
            let mut slot = self.current.write().unwrap_or_else(PoisonError::into_inner);
            // Поколение меняется под той же блокировкой: снимок видит клиента и номер согласованно.
            self.phase.send_modify(|p| {
                p.generation += 1;
                if !resumed {
                    p.window += 1;
                }
            });
            slot.replace(current)
        };
        drop(previous);
    }

    pub fn current<T>(&self, f: impl FnOnce(&Current) -> T) -> Option<T> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(f)
    }

    /// Пока жив guard, вызовы на закрытом соединении ждут реконнекта.
    pub fn reconnecting(&self) -> Reconnecting<'_> {
        self.phase.send_modify(|p| p.reconnects = true);
        Reconnecting(self)
    }

    pub fn kernel(self: &Arc<Self>) -> Option<Kernel> {
        let caller = self.current(|c| c.client.caller().clone())?;
        Some(Session::new(LiveSession { link: self.clone() }, caller))
    }

    fn snapshot(&self) -> Result<Live, Aux> {
        let slot = self.current.read().unwrap_or_else(PoisonError::into_inner);
        let current = slot
            .as_ref()
            .ok_or_else(|| Aux::Unavailable("module is not connected".to_owned()))?;
        let phase = *self.phase.borrow();
        Ok(Live {
            session: current.client.session(),
            peer: current.client.peer().clone(),
            generation: phase.generation,
            window: phase.window,
        })
    }

    /// Открытое соединение новее `stale`; не дождались (реконнекта нет или
    /// истёк `deadline`) — последнее, какое есть, и вызов упадёт с причиной обрыва.
    async fn open(&self, stale: Option<u64>, deadline: Instant) -> Result<Live, Aux> {
        let mut phase = self.phase.subscribe();
        loop {
            // Флаг читается до снимка: смена между ними разбудит `changed`.
            let reconnects = phase.borrow_and_update().reconnects;
            let live = self.snapshot()?;
            if stale != Some(live.generation) && !live.peer.is_closed() {
                return Ok(live);
            }
            if !reconnects {
                return Ok(live);
            }
            match tokio::time::timeout_at(deadline, phase.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => return Ok(live),
            }
        }
    }

    /// Вызов на текущем соединении; оборванный в полёте повторяется на новом.
    async fn run<T, E, Fut>(&self, mut attempt: impl FnMut(Rpc, Peer) -> Fut) -> Response<T, E>
    where
        Fut: Future<Output = Response<T, E>>,
    {
        let deadline = Instant::now() + CALL_RECONNECT_WAIT;
        let mut live = self.open(None, deadline).await?;
        loop {
            let result = attempt(live.session, live.peer.clone()).await;
            let Some(reason) = cut(&result, &live.peer) else {
                return result;
            };
            let next = self.open(Some(live.generation), deadline).await?;
            if next.generation == live.generation {
                return Err(lost(reason));
            }
            log::debug!(generation = next.generation; "call resent after reconnect");
            live = next;
        }
    }

    /// Мутация: id выделяется один раз и переживает реконнект.
    async fn mutate(&self, method: &str, params: Payload) -> Result<Payload, RpcError> {
        let deadline = Instant::now() + CALL_RECONNECT_WAIT;
        let mut live = self
            .open(None, deadline)
            .await
            .map_err(|aux| codec::aux(&aux))?;
        let id = live.peer.ids().next();
        loop {
            let result = live.peer.call_with_id(id, method, params.clone()).await;
            let reason = match &result {
                Err(error) if live.peer.close_reason().as_ref() == Some(error) => {
                    error.message.clone()
                }
                _ => return result,
            };
            let next = self
                .open(Some(live.generation), deadline)
                .await
                .map_err(|aux| codec::aux(&aux))?;
            if next.generation == live.generation {
                return Err(RpcError::new(ErrorCode::Unavailable, reason));
            }
            if next.window != live.window {
                log::warn!(id = id, method = method, reason = reason.as_str(); "mutation cut by a reconnect the kernel did not resume, outcome unknown");
                return Err(RpcError::new(
                    ErrorCode::Unavailable,
                    "connection lost mid-mutation; the kernel did not resume the session, outcome unknown",
                ));
            }
            log::debug!(id = id, method = method, generation = next.generation; "mutation resent after reconnect");
            live = next;
        }
    }
}

/// Причина закрытия, если вызов оборвался вместе с соединением, а не получил ответ.
fn cut<T, E>(result: &Response<T, E>, peer: &Peer) -> Option<String> {
    let Err(Generic::Aux(aux)) = result else {
        return None;
    };
    let reason = peer.close_reason()?;
    (reason.clone().into_aux() == *aux).then_some(reason.message)
}

/// Обрыв — `Aux::Unavailable` при любой причине закрытия (`11 §6`), вытеснение тоже.
fn lost<E>(reason: String) -> Generic<E> {
    Generic::Aux(Aux::Unavailable(reason))
}

pub(crate) struct Reconnecting<'a>(&'a Link);

impl Drop for Reconnecting<'_> {
    fn drop(&mut self) {
        self.0.phase.send_modify(|p| p.reconnects = false);
    }
}

/// Сессия `Kernel`: операции идут через текущее соединение модуля.
#[derive(Clone)]
pub struct LiveSession {
    link: Arc<Link>,
}

impl std::fmt::Debug for LiveSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveSession")
            .field(
                "session",
                &self.link.current(|c| c.welcome.session_id.clone()),
            )
            .finish_non_exhaustive()
    }
}

struct LiveRead<A, O, E, F> {
    link: Arc<Link>,
    op: F,
    _types: PhantomData<fn(A) -> (O, E)>,
}

impl<A, O, E, F, Fut> CallStep<A> for LiveRead<A, O, E, F>
where
    A: Clone + Send + 'static,
    O: Send + 'static,
    E: Send + 'static,
    F: Fn(Rpc, A) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response<O, E>> + Send,
{
    type Ok = O;
    type Err = E;

    fn call(&mut self, args: A) -> impl Future<Output = Response<O, E>> + Send {
        let (link, op) = (self.link.clone(), self.op.clone());
        async move { link.run(move |session, _| op(session, args.clone())).await }
    }
}

struct LiveMut<A, O, E> {
    link: Arc<Link>,
    method: String,
    _types: PhantomData<fn(A) -> (O, E)>,
}

impl<A, O, E> CallStep<A> for LiveMut<A, O, E>
where
    A: Serialize,
    O: DeserializeOwned + Send + 'static,
    E: DeserializeOwned + Send + 'static,
{
    type Ok = O;
    type Err = E;

    fn call(&mut self, args: A) -> impl Future<Output = Response<O, E>> + Send {
        let (link, method) = (self.link.clone(), self.method.clone());
        let params = codec::encode(&args);
        async move {
            let params = params.map_err(|e| invalid("arguments", e))?;
            let raw = link.mutate(&method, params).await.map_err(from_rpc)?;
            codec::decode(&raw).map_err(|e| invalid("malformed result", e))
        }
    }
}

fn invalid<E>(what: &str, e: impl std::fmt::Display) -> Generic<E> {
    Generic::Aux(Aux::Invalid(format!("{what}: {e}")))
}

struct LiveStream<A, T, E, F> {
    link: Arc<Link>,
    op: F,
    _types: PhantomData<fn(A) -> (T, E)>,
}

impl<A, T, E, F, Fut> StreamStep<A> for LiveStream<A, T, E, F>
where
    A: Clone + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
    F: Fn(Rpc, A) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response<Items<T, E>, E>> + Send,
{
    type Item = T;
    type Err = E;

    fn open(&mut self, args: A) -> impl Future<Output = Response<Items<T, E>, E>> + Send {
        let (link, op) = (self.link.clone(), self.op.clone());
        async move {
            link.run(move |session, peer| {
                let opened = op(session, args.clone());
                async move {
                    let items = opened.await?;
                    let items: Items<T, E> =
                        Box::pin(items.map(move |item| match cut(&item, &peer) {
                            Some(reason) => Err(lost(reason)),
                            None => item,
                        }));
                    Ok(items)
                }
            })
            .await
        }
    }
}

struct LiveUpload<A, O, E, F> {
    link: Arc<Link>,
    op: F,
    _types: PhantomData<fn(A) -> (O, E)>,
}

impl<A, O, E, F, Fut> UploadStep<A> for LiveUpload<A, O, E, F>
where
    A: Send + 'static,
    O: Send + 'static,
    E: Send + 'static,
    F: Fn(Rpc, A, ByteStream) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response<O, E>> + Send,
{
    type Ok = O;
    type Err = E;

    fn call(&mut self, args: A, body: ByteStream) -> impl Future<Output = Response<O, E>> + Send {
        let (link, op) = (self.link.clone(), self.op.clone());
        async move {
            let deadline = Instant::now() + CALL_RECONNECT_WAIT;
            let live = link.open(None, deadline).await?;
            let result = op(live.session, args, body).await;
            match cut(&result, &live.peer) {
                Some(reason) => Err(lost(reason)),
                None => result,
            }
        }
    }
}

macro_rules! live_step {
    (read $ns:ident $Trait:ident $op:ident) => {
        fn $op(
            &mut self,
        ) -> impl CallStep<
            eva_protocol::requests::$ns::$op::Args,
            Ok = eva_protocol::requests::$ns::$op::Ok,
            Err = eva_protocol::requests::$ns::$op::Err,
        > {
            LiveRead {
                link: self.link.clone(),
                op: |mut session: Rpc, args: eva_protocol::requests::$ns::$op::Args| async move {
                    session.$ns().$op().call(args).await
                },
                _types: PhantomData,
            }
        }
    };
    (mut $ns:ident $Trait:ident $op:ident) => {
        fn $op(
            &mut self,
        ) -> impl CallStep<
            eva_protocol::requests::$ns::$op::Args,
            Ok = eva_protocol::requests::$ns::$op::Ok,
            Err = eva_protocol::requests::$ns::$op::Err,
        > {
            LiveMut {
                link: self.link.clone(),
                method: format!(
                    "{}.{}",
                    stringify!($ns),
                    eva_core::service::op_name(stringify!($op))
                ),
                _types: PhantomData,
            }
        }
    };
    (stream $ns:ident $Trait:ident $op:ident) => {
        fn $op(
            &mut self,
        ) -> impl StreamStep<
            eva_protocol::requests::$ns::$op::Args,
            Item = eva_protocol::requests::$ns::$op::Item,
            Err = eva_protocol::requests::$ns::$op::Err,
        > {
            LiveStream {
                link: self.link.clone(),
                op: |mut session: Rpc, args: eva_protocol::requests::$ns::$op::Args| async move {
                    session.$ns().$op().open(args).await
                },
                _types: PhantomData,
            }
        }
    };
    (upload $ns:ident $Trait:ident $op:ident) => {
        fn $op(
            &mut self,
        ) -> impl UploadStep<
            eva_protocol::requests::$ns::$op::Args,
            Ok = eva_protocol::requests::$ns::$op::Ok,
            Err = eva_protocol::requests::$ns::$op::Err,
        > {
            LiveUpload {
                link: self.link.clone(),
                op: |mut session: Rpc,
                     args: eva_protocol::requests::$ns::$op::Args,
                     body: ByteStream| async move {
                    session.$ns().$op().call(args, body).await
                },
                _types: PhantomData,
            }
        }
    };
}

macro_rules! live_impls {
    ($($ns:ident: $Trait:ident { $($kind:ident $op:ident),* $(,)? }),* $(,)?) => {
        $(
            impl eva_core::service::$ns::$Trait for LiveSession {
                $(live_step!($kind $ns $Trait $op);)*
            }
        )*

        /// Метки списка: сверяются с `NAMESPACES` ядра в тесте.
        #[cfg(test)]
        const LABELS: &[(&str, &[(&str, &str)])] = &[$(
            (stringify!($ns), &[$((stringify!($kind), eva_core::service::op_name(stringify!($op)))),*]),
        )*];
    };
}

// Тот же список, что у `eva-rpc`: пропуск операции не скомпилируется, метка
// задаёт форму шага и способ повтора после обрыва.
live_impls! {
    surfaces: Surfaces { mut create, read get, read list, mut update },
    folders: Folders { mut create, mut update, mut delete, read list },
    chats: Chats {
        mut ingest, mut create, read get, read list, mut update, mut r#move,
        mut delete, mut restore, read search, read history_read, mut compact,
        mut notify,
    },
    turns: Turns {
        mut start, stream attach, mut append_silent, mut delivered, mut cancel,
        mut respond,
    },
    persons: Persons { mut resolve, read list, mut rename, mut merge, mut whitelist },
    memory: Memory { read list, read get, read history, mut restore },
    notes: Notes { read list },
    tools: Tools { read catalog },
    models: Models { read list },
    cron: Cron { read list, read runs },
    sleep: Sleep { read runs, read phase_runs },
    agents: Agents { read list },
    stats: Stats {
        read tools_usage, read spend, read spend_series, read chat_spend, read health,
    },
    modules: Modules { read list, mut token_issue, mut token_revoke },
    hands_grants: HandsGrants { read list, mut revoke },
    blobs: Blobs { upload put, stream get, read stat },
    inference: Inference { mut complete },
    fs: Fs { read read, mut write, read list, read stat, mut remove },
    sqlite: Sqlite { mut exec, read query, mut tx },
    kv: Kv { read get, mut set, mut del, read list },
}

#[cfg(test)]
mod tests {
    use eva_core::{NAMESPACES, OpKind};

    use super::LABELS;

    #[test]
    fn labels_match_the_library() {
        let ours: Vec<(&str, Vec<(&str, OpKind)>)> = LABELS
            .iter()
            .map(|(ns, ops)| {
                let ops = ops
                    .iter()
                    .map(|(label, op)| {
                        let kind = match *label {
                            "read" => OpKind::Read,
                            "mut" => OpKind::Mutating,
                            "stream" => OpKind::Stream,
                            "upload" => OpKind::Upload,
                            other => panic!("unknown label {other}"),
                        };
                        (*op, kind)
                    })
                    .collect();
                (*ns, ops)
            })
            .collect();
        let library: Vec<(&str, Vec<(&str, OpKind)>)> = NAMESPACES
            .iter()
            .map(|(ns, ops)| (*ns, ops.iter().map(|op| (op.name, op.kind)).collect()))
            .collect();
        assert_eq!(ours, library);
    }
}
