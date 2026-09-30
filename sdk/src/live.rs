//! `Kernel` модуля: каждый вызов берёт текущее соединение, поэтому ручка,
//! взятая до смены соединения, после неё ведёт в новую сессию.
//!
//! Вызов на закрытом соединении ждёт нового до `CALL_RECONNECT_WAIT`, пока
//! новое возможно: у клиента жив `run`, у сервиса — приём соединений ядра.
//! Оборванные в полёте чтение и открытие потока повторяются на новом
//! соединении, мутация — с тем же id запроса, только если ядро продолжило
//! окно идемпотентности (`welcome.resumed`, `06 §5`) и не исполнит её второй
//! раз; иначе (рестарт ядра) исход неизвестен, и она отвечает
//! `Aux::Unavailable`. Загрузка отвечает `Aux::Unavailable` (`11 §6`): её
//! тело уже прочитано.

use std::{
    marker::PhantomData,
    sync::{Arc, PoisonError, RwLock},
    time::Duration,
};

use futures::StreamExt as _;
use serde::{Serialize, de::DeserializeOwned};
use tokio::{sync::watch, time::Instant};

use eva::fut::Fut;
use eva_core::{CallStep, Items, Session, service::op_name};
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

/// Текущее соединение модуля; меняется с каждым новым.
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
    /// Новое соединение возможно: только тогда закрытое стоит ждать.
    reconnects: bool,
    /// Модуль остановлен: новых соединений не будет.
    closed: bool,
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
                closed: false,
            }),
        }
    }

    /// Новое соединение — текущее; ждущие вызовы просыпаются. Отдаёт
    /// прежнее; после `close` новое не ставится, а закрывается.
    pub fn install(&self, current: Current) -> Option<Current> {
        let resumed = current.welcome.resumed;
        let mut slot = self.current.write().unwrap_or_else(PoisonError::into_inner);
        if self.phase.borrow().closed {
            current.client.peer().close(shutting_down());
            return None;
        }
        // Поколение меняется под той же блокировкой: снимок видит клиента и номер согласованно.
        self.phase.send_modify(|p| {
            p.generation += 1;
            if !resumed {
                p.window += 1;
            }
        });
        slot.replace(current)
    }

    /// Остановка модуля: текущее соединение закрывается, новые не ставятся.
    pub fn close(&self) {
        let slot = self.current.write().unwrap_or_else(PoisonError::into_inner);
        self.phase.send_modify(|p| p.closed = true);
        if let Some(current) = slot.as_ref() {
            current.client.peer().close(shutting_down());
        }
    }

    pub fn is_closed(&self) -> bool {
        self.phase.borrow().closed
    }

    /// Ждёт первого соединения; `None` — модуль остановлен раньше.
    pub async fn first(self: &Arc<Self>) -> Option<Kernel> {
        let mut phase = self.phase.subscribe();
        _ = phase.wait_for(|p| p.generation > 0 || p.closed).await;
        self.kernel()
    }

    pub fn current<T>(&self, f: impl FnOnce(&Current) -> T) -> Option<T> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(f)
    }

    /// Пока жив guard, вызовы на закрытом соединении ждут нового.
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
            let reconnects = {
                let phase = phase.borrow_and_update();
                phase.reconnects && !phase.closed
            };
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
    async fn run<T, E, R>(&self, mut attempt: impl FnMut(Rpc, Peer) -> R) -> Response<T, E>
    where
        R: Future<Output = Response<T, E>>,
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

pub(crate) fn shutting_down() -> RpcError {
    RpcError::new(ErrorCode::Unavailable, "module is shutting down")
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

struct LiveRead<F> {
    link: Arc<Link>,
    op: F,
}

impl<A, O, E, F, R> CallStep<A> for LiveRead<F>
where
    A: Clone + Send + 'static,
    O: Send + 'static,
    E: Send + 'static,
    F: Fn(Rpc, A) -> R + Clone + Send + Sync + 'static,
    R: Fut<Output = Response<O, E>>,
{
    type Output = Response<O, E>;

    fn call(&mut self, args: A) -> impl Fut<Output = Self::Output> {
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
    type Output = Response<O, E>;

    fn call(&mut self, args: A) -> impl Fut<Output = Self::Output> {
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

struct LiveStream<F> {
    link: Arc<Link>,
    op: F,
}

impl<A, T, E, F, R> CallStep<A> for LiveStream<F>
where
    A: Clone + Send + 'static,
    T: Send + 'static,
    E: Send + 'static,
    F: Fn(Rpc, A) -> R + Clone + Send + Sync + 'static,
    R: Fut<Output = Response<Items<T, E>, E>>,
{
    type Output = Response<Items<T, E>, E>;

    fn call(&mut self, args: A) -> impl Fut<Output = Self::Output> {
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

struct LiveUpload<F> {
    link: Arc<Link>,
    op: F,
}

impl<A, O, E, F, R> CallStep<A> for LiveUpload<F>
where
    A: Send + 'static,
    O: Send + 'static,
    E: Send + 'static,
    F: Fn(Rpc, A) -> R + Clone + Send + Sync + 'static,
    R: Fut<Output = Response<O, E>>,
{
    type Output = Response<O, E>;

    fn call(&mut self, args: A) -> impl Fut<Output = Self::Output> {
        let (link, op) = (self.link.clone(), self.op.clone());
        async move {
            let deadline = Instant::now() + CALL_RECONNECT_WAIT;
            let live = link.open(None, deadline).await?;
            let result = op(live.session, args).await;
            match cut(&result, &live.peer) {
                Some(reason) => Err(lost(reason)),
                None => result,
            }
        }
    }
}

/// Мутация идёт мимо сессии соединения (`$method`, не `$op`), чтобы её id запроса пережил реконнект.
macro_rules! live_step {
    ([] $link:expr, $method:expr, $op:expr) => {
        LiveRead {
            link: $link,
            op: $op,
        }
    };
    ([mutating] $link:expr, $method:expr, $op:expr) => {
        LiveMut {
            link: $link,
            method: $method,
            _types: PhantomData,
        }
    };
    ([stream] $link:expr, $method:expr, $op:expr) => {
        LiveStream {
            link: $link,
            op: $op,
        }
    };
    ([upload] $link:expr, $method:expr, $op:expr) => {
        LiveUpload {
            link: $link,
            op: $op,
        }
    };
}

macro_rules! live_impls {
    ($(
        $segment:ident {$(
            $accessor:ident : $Trait:ident {$(
                $(#[$flag:ident])*
                $method:ident = $endpoint:ident {
                    args: $Args:ty,
                    ok: $Ok:ty,
                    err: $Err:ty,
                    output: $Output:ty,
                }
            )*}
        )*}
    )*) => {$($(
        impl eva_core::service::$segment::$Trait for LiveSession {$(
            fn $method(&mut self) -> impl CallStep<$Args, Output = $Output> {
                live_step!(
                    [$($flag)*] self.link.clone(),
                    format!("{}.{}", stringify!($segment), op_name(stringify!($endpoint))),
                    |mut session: Rpc, args: $Args| async move {
                        session.$accessor().$method().call(args).await
                    }
                )
            }
        )*}
    )*)*};
}

eva_core::for_each_op!(live_impls);
