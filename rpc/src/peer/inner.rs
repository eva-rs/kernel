//! Задачи соединения: читатель, писатель, ping, уведомления и обработчики запросов.

use std::{
    collections::HashMap,
    mem,
    panic::AssertUnwindSafe,
    pin::pin,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures::{FutureExt as _, SinkExt as _, StreamExt as _};
use tokio::{
    sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::AbortHandle,
    time::{Instant, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;

use eva_protocol::wire::{
    ErrorCode, RpcError,
    frame::{
        Cancel, Frame, Notify as NotifyFrame, Payload, Request, Response, StreamEnd,
        StreamHeartbeat, StreamItem,
    },
    handshake::Ping,
    method,
};

use crate::{
    codec,
    config::Config,
    peer::{Call, IdSeq, Kind, Service, body::Body},
    transport::{Conn, Outbound, Rx, TransportError, Tx, meter::Meter},
};

/// Сколько писатель дожимает последние кадры в закрывающийся сокет.
const FLUSH_ON_CLOSE: Duration = Duration::from_secs(5);

/// Нулевой период из конфига не должен ронять таймер.
const MIN_TICK: Duration = Duration::from_millis(1);

/// Срок, который считается «никогда»: абсурдный таймаут не переполняет `Instant`.
const FAR: Duration = Duration::from_secs(60 * 60 * 24 * 365);

/// Байт в очереди полосы запросов и полосы потоков: запрос не ждёт за
/// десятками мегабайт чужих чанков.
const LANE_BYTES: usize = 2 << 20;

/// Мелких управляющих кадров в очереди больше — пир не читает сокет.
const CTRL_BACKLOG: usize = 4 << 20;

/// Ответов, ждущих записи, в `max_frame`: сверх ответ заменяется `quota`.
const REPLY_FRAMES: usize = 4;

pub(super) fn after(from: Instant, span: Duration) -> Instant {
    from.checked_add(span).unwrap_or(from + FAR)
}

fn ticker(every: Duration) -> tokio::time::Interval {
    let every = every.clamp(MIN_TICK, FAR);
    let mut tick = tokio::time::interval_at(Instant::now() + every, every);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tick
}

/// Кадр в очереди писателя; пермит держит место в бюджете до записи в сокет.
struct Queued {
    raw: bytes::Bytes,
    permit: Option<OwnedSemaphorePermit>,
}

/// Байтовый бюджет очереди. Кадр крупнее бюджета берёт его целиком.
struct Budget {
    permits: Arc<Semaphore>,
    size: usize,
}

impl Budget {
    fn new(size: usize) -> Self {
        let size = size.clamp(1, usize::try_from(u32::MAX).unwrap_or(usize::MAX));
        Self {
            permits: Arc::new(Semaphore::new(size)),
            size,
        }
    }

    fn cost(&self, raw: &[u8]) -> u32 {
        u32::try_from(raw.len().min(self.size)).unwrap_or(u32::MAX)
    }

    async fn take(&self, raw: &[u8]) -> Option<OwnedSemaphorePermit> {
        self.permits
            .clone()
            .acquire_many_owned(self.cost(raw))
            .await
            .ok()
    }

    fn try_take(&self, raw: &[u8]) -> Option<OwnedSemaphorePermit> {
        self.permits
            .clone()
            .try_acquire_many_owned(self.cost(raw))
            .ok()
    }
}

/// Очередь кадров данных с байтовым бюджетом.
struct Lane {
    tx: mpsc::UnboundedSender<Queued>,
    budget: Budget,
}

pub(super) struct Inner {
    pub cfg: Config,
    pub remote: String,
    pub ids: IdSeq,
    meter: Arc<Meter>,
    /// `response`, `ping`, heartbeat, `cancel`: мимо очередей данных.
    ctrl: mpsc::UnboundedSender<Queued>,
    /// Байт мелких управляющих кадров в `ctrl`; ответы учитывает `replies`.
    ctrl_bytes: AtomicUsize,
    replies: Budget,
    /// Свои запросы и уведомления.
    calls: Lane,
    /// Элементы и концы потоков, загрузки целиком: порядок внутри потока важен.
    bulk: Lane,
    inflight: Arc<Semaphore>,
    streams: Arc<Semaphore>,
    state: Mutex<State>,
    pub closed: CancellationToken,
    reason: OnceLock<RpcError>,
}

#[derive(Default)]
struct State {
    /// Закрытие началось: новые вызовы и обработчики не заводятся.
    closing: bool,
    /// Свои исходящие вызовы.
    pending: HashMap<u64, Pending>,
    /// Входящие запросы, которые обслуживаются сейчас.
    served: HashMap<u64, Served>,
    /// Тела загрузок пира: чанки к `id` из его пространства.
    uploads: HashMap<u64, Feed>,
}

pub(super) enum Pending {
    Unary {
        tx: oneshot::Sender<Result<Payload, RpcError>>,
        /// `stream_heartbeat` на id запроса: пир ещё работает над ответом.
        alive: Arc<Notify>,
    },
    Stream(Feed),
}

struct Served {
    kind: Kind,
    task: AbortHandle,
    /// `cancel` не прерывает обработчик (`Service::survives_cancel`).
    detach: bool,
}

pub(super) enum Event {
    Item(Payload),
    Heartbeat,
}

/// Конец потока: `None` внутри — чистый, выставляется до закрытия канала.
pub(super) type End = OnceLock<Option<RpcError>>;

/// Приёмная сторона потока: ограниченный буфер плюс слот конца.
pub(super) struct Feed {
    tx: mpsc::Sender<Event>,
    end: Arc<End>,
    next_seq: u64,
}

enum Push {
    Ok,
    OutOfOrder,
    Overflow,
    Gone,
}

impl Feed {
    pub fn new(capacity: usize) -> (Self, mpsc::Receiver<Event>, Arc<End>) {
        let (tx, rx) = mpsc::channel(capacity);
        let end = Arc::new(End::new());
        (
            Self {
                tx,
                end: end.clone(),
                next_seq: 0,
            },
            rx,
            end,
        )
    }

    pub fn finish(self, error: Option<RpcError>) {
        _ = self.end.set(error);
    }

    fn push(&mut self, seq: u64, item: Payload) -> Push {
        if seq != self.next_seq {
            return Push::OutOfOrder;
        }
        match self.tx.try_send(Event::Item(item)) {
            Ok(()) => {
                self.next_seq += 1;
                Push::Ok
            }
            Err(mpsc::error::TrySendError::Full(_)) => Push::Overflow,
            Err(mpsc::error::TrySendError::Closed(_)) => Push::Gone,
        }
    }

    /// Полный буфер и так означает живой поток: heartbeat можно потерять.
    fn heartbeat(&self) {
        _ = self.tx.try_send(Event::Heartbeat);
    }
}

impl Push {
    fn error(self) -> Option<RpcError> {
        match self {
            Self::Ok => None,
            Self::OutOfOrder => Some(RpcError::invalid_params("stream item out of order")),
            Self::Overflow => Some(RpcError::new(ErrorCode::Quota, "stream buffer overflow")),
            Self::Gone => Some(RpcError::cancelled()),
        }
    }
}

/// Регистрация своего вызова: drop до ответа снимает её и шлёт `cancel`.
pub(super) struct Guard {
    inner: Arc<Inner>,
    id: u64,
    armed: bool,
}

impl Guard {
    /// Ответ получен — отменять нечего.
    pub fn disarm(&mut self) {
        self.armed = false;
    }

    /// Запрос не ушёл: снять регистрацию без `cancel`.
    pub fn abandon(&mut self) {
        self.armed = false;
        self.inner.lock().pending.remove(&self.id);
    }

    pub fn fire(&mut self) {
        if !mem::take(&mut self.armed) {
            return;
        }
        let removed = self.inner.lock().pending.remove(&self.id).is_some();
        if removed && !self.inner.is_closed() {
            self.inner.send_ctrl(&Frame::Cancel(Cancel { id: self.id }));
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.fire();
    }
}

impl Inner {
    pub fn spawn<S: Service>(conn: Conn, cfg: Config, ids: IdSeq, service: Arc<S>) -> Arc<Self> {
        let Conn {
            tx,
            rx,
            remote,
            meter,
        } = conn;
        let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel();
        let (calls_tx, calls_rx) = mpsc::unbounded_channel();
        let (bulk_tx, bulk_rx) = mpsc::unbounded_channel();
        meter.touch_rx();
        meter.touch_tx();

        let inner = Arc::new(Self {
            remote,
            ids,
            meter,
            ctrl: ctrl_tx,
            ctrl_bytes: AtomicUsize::new(0),
            replies: Budget::new(cfg.max_frame().saturating_mul(REPLY_FRAMES)),
            calls: Lane {
                tx: calls_tx,
                budget: Budget::new(LANE_BYTES),
            },
            bulk: Lane {
                tx: bulk_tx,
                budget: Budget::new(LANE_BYTES),
            },
            inflight: Arc::new(Semaphore::new(cfg.max_inflight())),
            streams: Arc::new(Semaphore::new(cfg.max_streams())),
            cfg,
            state: Mutex::default(),
            closed: CancellationToken::new(),
            reason: OnceLock::new(),
        });

        tokio::spawn(writer(inner.clone(), tx, ctrl_rx, calls_rx, bulk_rx));
        tokio::spawn(reader(inner.clone(), rx, service));
        tokio::spawn(pinger(inner.clone()));

        inner
    }

    // Паника под замком не портит карты: каждая операция над ними атомарна.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.is_cancelled()
    }

    pub fn reason(&self) -> RpcError {
        self.reason
            .get()
            .cloned()
            .unwrap_or_else(|| codec::unavailable("connection closed"))
    }

    fn encode(&self, frame: &Frame) -> Result<bytes::Bytes, RpcError> {
        let raw = codec::encode_frame(frame).map_err(|e| RpcError::new(ErrorCode::Internal, e))?;
        if raw.len() > self.cfg.max_frame() {
            return Err(RpcError::invalid_params(format_args!(
                "frame of {} bytes exceeds max_frame",
                raw.len()
            )));
        }
        Ok(raw)
    }

    pub fn send_ctrl(&self, frame: &Frame) {
        match self.encode(frame) {
            Ok(raw) => self.push_ctrl(raw),
            Err(e) => {
                log::warn!(remote = self.remote.as_str(), error:% = e; "control frame dropped")
            }
        }
    }

    fn push_ctrl(&self, raw: bytes::Bytes) {
        let len = raw.len();
        if self.ctrl_bytes.fetch_add(len, Ordering::Relaxed) + len > CTRL_BACKLOG {
            self.ctrl_bytes.fetch_sub(len, Ordering::Relaxed);
            self.close(RpcError::new(
                ErrorCode::Quota,
                "control backlog exceeded: the peer does not read",
            ));
            return;
        }
        if self.ctrl.send(Queued { raw, permit: None }).is_err() {
            self.ctrl_bytes.fetch_sub(len, Ordering::Relaxed);
        }
    }

    /// Свой запрос или уведомление.
    pub async fn send_call(&self, frame: &Frame) -> Result<(), RpcError> {
        self.push(&self.calls, frame).await
    }

    /// Кадр потока или загрузки.
    pub async fn send_bulk(&self, frame: &Frame) -> Result<(), RpcError> {
        self.push(&self.bulk, frame).await
    }

    async fn push(&self, lane: &Lane, frame: &Frame) -> Result<(), RpcError> {
        let raw = self.encode(frame)?;
        let permit = tokio::select! {
            biased;
            () = self.closed.cancelled() => return Err(self.reason()),
            permit = lane.budget.take(&raw) => permit.ok_or_else(|| self.reason())?,
        };
        lane.tx
            .send(Queued {
                raw,
                permit: Some(permit),
            })
            .map_err(|_| self.reason())
    }

    /// Ответ, не влезший в `max_frame`, заменяется ошибкой; ответ сверх
    /// бюджета неотправленных — `quota`, чтобы нечитающий пир не копил память ядра.
    fn respond(&self, id: u64, result: Result<Payload, RpcError>) {
        let response = match result {
            Ok(result) => Response::ok(id, result),
            Err(error) => Response::err(id, error),
        };
        let raw = match self.encode(&Frame::Response(response)) {
            Ok(raw) => raw,
            Err(error) => {
                self.send_ctrl(&Frame::Response(Response::err(id, error)));
                return;
            }
        };
        match self.replies.try_take(&raw) {
            Some(permit) => {
                _ = self.ctrl.send(Queued {
                    raw,
                    permit: Some(permit),
                });
            }
            None => {
                log::warn!(remote = self.remote.as_str(), id = id, bytes = raw.len(); "response dropped: the peer does not read");
                self.send_ctrl(&Frame::Response(Response::err(
                    id,
                    RpcError::new(
                        ErrorCode::Quota,
                        "response backlog exceeded: the peer does not read",
                    ),
                )));
            }
        }
    }

    pub fn register(self: &Arc<Self>, id: u64, pending: Pending) -> Result<Guard, RpcError> {
        let mut state = self.lock();
        if state.closing {
            return Err(self.reason());
        }
        state.pending.insert(id, pending);
        Ok(Guard {
            inner: self.clone(),
            id,
            armed: true,
        })
    }

    /// Снимает обслуживаемый запрос; ответ шлёт тот, кто снял.
    fn take_served(&self, id: u64) -> bool {
        self.lock().served.remove(&id).is_some()
    }

    fn is_served(&self, id: u64) -> bool {
        self.lock().served.contains_key(&id)
    }

    /// Закрытие по своей причине: пир узнаёт её уведомлением `close`.
    pub fn close(&self, reason: RpcError) {
        self.shut(reason, true);
    }

    fn shut(&self, reason: RpcError, tell_peer: bool) {
        if self.reason.set(reason.clone()).is_err() {
            return;
        }

        let (pending, served, uploads) = {
            let mut state = self.lock();
            state.closing = true;
            (
                mem::take(&mut state.pending),
                mem::take(&mut state.served),
                mem::take(&mut state.uploads),
            )
        };

        // Потоки, которые отдаём мы, узнают причину до закрытия сокета;
        // унарные обработчики доживают сами — их итог нужен окну идемпотентности.
        for (id, served) in served {
            if served.kind == Kind::Stream {
                served.task.abort();
                self.send_ctrl(&Frame::StreamEnd(StreamEnd {
                    id,
                    error: Some(reason.clone()),
                }));
            }
        }
        if tell_peer {
            match codec::encode(&reason) {
                Ok(params) => self.send_ctrl(&Frame::Notify(NotifyFrame {
                    method: method::CLOSE.to_owned(),
                    params,
                })),
                Err(e) => log::warn!(error:% = e; "close reason dropped"),
            }
        }
        self.closed.cancel();

        for (_, pending) in pending {
            match pending {
                Pending::Unary { tx, .. } => _ = tx.send(Err(reason.clone())),
                Pending::Stream(feed) => feed.finish(Some(reason.clone())),
            }
        }
        for (_, feed) in uploads {
            feed.finish(Some(reason.clone()));
        }

        log::debug!(remote = self.remote.as_str(), reason:% = reason; "connection closed");
    }

    fn serve<S: Service>(self: &Arc<Self>, service: &Arc<S>, request: Request) {
        let Request { id, method, params } = request;
        let Some(kind) = service.kind(&method) else {
            log::debug!(remote = self.remote.as_str(), method = codec::shown(&method).as_str(); "unknown method");
            self.respond(id, Err(RpcError::unknown_method(&method)));
            return;
        };
        let limit = match kind {
            Kind::Stream => &self.streams,
            Kind::Unary | Kind::Upload => &self.inflight,
        };

        let mut state = self.lock();
        if state.closing {
            return;
        }
        if state.served.contains_key(&id) {
            log::warn!(remote = self.remote.as_str(), id = id; "duplicate in-flight request id ignored");
            return;
        }
        let Ok(permit) = limit.clone().try_acquire_owned() else {
            drop(state);
            log::debug!(remote = self.remote.as_str(), method = method.as_str(); "request refused: too many in flight");
            let error = RpcError::new(ErrorCode::Quota, "too many requests in flight");
            self.send_ctrl(&match kind {
                Kind::Stream => Frame::StreamEnd(StreamEnd {
                    id,
                    error: Some(error),
                }),
                Kind::Unary | Kind::Upload => Frame::Response(Response::err(id, error)),
            });
            return;
        };

        let detach = kind == Kind::Unary && service.survives_cancel(&method);
        let call = Call { id, method, params };
        let task = match kind {
            Kind::Unary => tokio::spawn(unary(self.clone(), service.clone(), call, permit)),
            Kind::Stream => tokio::spawn(stream(self.clone(), service.clone(), call, permit)),
            Kind::Upload => {
                let (feed, rx, end) = Feed::new(self.cfg.stream_buffer());
                state.uploads.insert(id, feed);
                let body = Body::new(rx, end, *self.cfg.stream_idle_timeout);
                tokio::spawn(upload(self.clone(), service.clone(), call, body, permit))
            }
        };
        state.served.insert(
            id,
            Served {
                kind,
                task: task.abort_handle(),
                detach,
            },
        );
    }

    fn on_response(&self, response: Response) {
        let id = response.id;
        let pending = self.lock().pending.remove(&id);
        match pending {
            Some(Pending::Unary { tx, .. }) => _ = tx.send(response.into_result()),
            Some(Pending::Stream(feed)) => {
                let error = response
                    .into_result()
                    .err()
                    .unwrap_or_else(|| RpcError::invalid_params("response to a stream request"));
                feed.finish(Some(error));
            }
            None => {
                log::debug!(remote = self.remote.as_str(), id = id; "response to an unknown request")
            }
        }
    }

    fn on_item(&self, item: StreamItem) {
        let StreamItem { id, seq, item } = item;
        let mut state = self.lock();

        if let Some(Pending::Stream(feed)) = state.pending.get_mut(&id) {
            let Some(error) = feed.push(seq, item).error() else {
                return;
            };
            let Some(Pending::Stream(feed)) = state.pending.remove(&id) else {
                return;
            };
            drop(state);

            log::debug!(remote = self.remote.as_str(), id = id, error:% = error; "stream broken on receive");
            feed.finish(Some(error));
            self.send_ctrl(&Frame::Cancel(Cancel { id }));
            return;
        }

        if let Some(feed) = state.uploads.get_mut(&id)
            && let Some(error) = feed.push(seq, item).error()
            && let Some(feed) = state.uploads.remove(&id)
        {
            drop(state);
            feed.finish(Some(error));
        }
    }

    fn on_heartbeat(&self, id: u64) {
        let state = self.lock();
        match state.pending.get(&id) {
            Some(Pending::Stream(feed)) => feed.heartbeat(),
            Some(Pending::Unary { alive, .. }) => alive.notify_one(),
            None => {
                if let Some(feed) = state.uploads.get(&id) {
                    feed.heartbeat();
                }
            }
        }
    }

    fn on_end(&self, end: StreamEnd) {
        let StreamEnd { id, error } = end;
        let mut state = self.lock();
        if let Some(pending) = state.pending.remove(&id) {
            drop(state);
            match pending {
                Pending::Stream(feed) => feed.finish(error),
                Pending::Unary { tx, .. } => {
                    let error = error.unwrap_or_else(|| {
                        RpcError::invalid_params("stream_end for a unary request")
                    });
                    _ = tx.send(Err(error));
                }
            }
        } else if let Some(feed) = state.uploads.remove(&id) {
            drop(state);
            feed.finish(error);
        }
    }

    fn on_cancel(&self, id: u64) {
        let (served, upload) = {
            let mut state = self.lock();
            (state.served.remove(&id), state.uploads.remove(&id))
        };
        if let Some(feed) = upload {
            feed.finish(Some(RpcError::cancelled()));
        }
        let Some(served) = served else {
            return;
        };
        if !served.detach {
            served.task.abort();
        }

        let frame = match served.kind {
            Kind::Stream => Frame::StreamEnd(StreamEnd {
                id,
                error: Some(RpcError::cancelled()),
            }),
            Kind::Unary | Kind::Upload => Frame::Response(Response::err(id, RpcError::cancelled())),
        };
        self.send_ctrl(&frame);
    }

    /// Пока обработчик работает, пир раз в `ping_interval` получает
    /// heartbeat на id запроса и не считает долгий ответ потерянным.
    async fn beating<T>(&self, id: u64, work: impl Future<Output = T>) -> T {
        let mut work = pin!(work);
        let mut tick = ticker(*self.cfg.ping_interval);
        loop {
            tokio::select! {
                biased;
                out = &mut work => return out,
                _ = tick.tick() => {
                    if self.is_served(id) {
                        self.send_ctrl(&Frame::StreamHeartbeat(StreamHeartbeat { id }));
                    }
                }
            }
        }
    }
}

async fn guarded<T>(fut: impl Future<Output = Result<T, RpcError>>) -> Result<T, RpcError> {
    AssertUnwindSafe(fut)
        .catch_unwind()
        .await
        .unwrap_or_else(|_| Err(RpcError::new(ErrorCode::Internal, "handler panicked")))
}

async fn unary<S: Service>(
    inner: Arc<Inner>,
    service: Arc<S>,
    call: Call,
    _permit: OwnedSemaphorePermit,
) {
    let id = call.id;
    let result = inner.beating(id, guarded(service.unary(call))).await;
    if inner.take_served(id) {
        inner.respond(id, result);
    }
}

async fn upload<S: Service>(
    inner: Arc<Inner>,
    service: Arc<S>,
    call: Call,
    body: Body,
    _permit: OwnedSemaphorePermit,
) {
    let id = call.id;
    let result = inner
        .beating(id, guarded(service.upload(call, Box::pin(body))))
        .await;
    // Обработчик мог ответить до конца тела: остаток чанков пира уходит в никуда.
    inner.lock().uploads.remove(&id);
    if inner.take_served(id) {
        inner.respond(id, result);
    }
}

async fn stream<S: Service>(
    inner: Arc<Inner>,
    service: Arc<S>,
    call: Call,
    _permit: OwnedSemaphorePermit,
) {
    let id = call.id;
    let items = match guarded(service.stream(call)).await {
        Ok(items) => items,
        Err(error) => {
            if inner.take_served(id) {
                _ = inner
                    .send_bulk(&Frame::StreamEnd(StreamEnd {
                        id,
                        error: Some(error),
                    }))
                    .await;
            }
            return;
        }
    };

    // Первый heartbeat — «открыт»: вызывающий отличает отказ до первого
    // элемента от пустого, но живого потока.
    inner.send_ctrl(&Frame::StreamHeartbeat(StreamHeartbeat { id }));

    let mut heartbeat = ticker(*inner.cfg.ping_interval);

    let mut items = AssertUnwindSafe(items).catch_unwind();
    let mut seq = 0;
    let error = loop {
        tokio::select! {
            biased;
            next = items.next() => match next {
                Some(Ok(Ok(item))) => {
                    let frame = Frame::StreamItem(StreamItem { id, seq, item });
                    if let Err(e) = inner.send_bulk(&frame).await {
                        break Some(e);
                    }
                    seq += 1;
                    heartbeat.reset();
                }
                Some(Ok(Err(error))) => break Some(error),
                Some(Err(_)) => break Some(RpcError::new(ErrorCode::Internal, "stream panicked")),
                None => break None,
            },
            _ = heartbeat.tick() => inner.send_ctrl(&Frame::StreamHeartbeat(StreamHeartbeat { id })),
        }
    };

    if inner.take_served(id) {
        _ = inner
            .send_bulk(&Frame::StreamEnd(StreamEnd { id, error }))
            .await;
    }
}

/// Запросы и данные потоков — поровну: ни одна полоса не голодает.
async fn next_data(
    calls: &mut mpsc::UnboundedReceiver<Queued>,
    bulk: &mut mpsc::UnboundedReceiver<Queued>,
) -> Option<Queued> {
    tokio::select! {
        Some(queued) = calls.recv() => Some(queued),
        Some(queued) = bulk.recv() => Some(queued),
        else => None,
    }
}

async fn writer(
    inner: Arc<Inner>,
    mut tx: Tx,
    mut ctrl: mpsc::UnboundedReceiver<Queued>,
    mut calls: mpsc::UnboundedReceiver<Queued>,
    mut bulk: mpsc::UnboundedReceiver<Queued>,
) {
    let stall = *inner.cfg.ping_timeout;
    loop {
        let Queued { raw, permit } = tokio::select! {
            biased;
            Some(queued) = ctrl.recv() => {
                if queued.permit.is_none() {
                    inner.ctrl_bytes.fetch_sub(queued.raw.len(), Ordering::Relaxed);
                }
                queued
            }
            () = inner.closed.cancelled() => break,
            queued = next_data(&mut calls, &mut bulk) => match queued {
                Some(queued) => queued,
                None => break,
            },
        };

        // Закрытие прерывает только запись, которая встала: взятый кадр
        // закрытия (`stream_end{reason}`) иначе потерялся бы. Встала — значит
        // сокет не принял ни байта за `ping_timeout`.
        inner.meter.touch_tx();
        let sent = {
            let mut send = tx.send(Outbound::Frame(raw));
            loop {
                let stalled = after(inner.meter.last_tx(), stall);
                tokio::select! {
                    biased;
                    sent = &mut send => break Some(sent),
                    () = inner.closed.cancelled() => break None,
                    () = tokio::time::sleep_until(stalled) => {
                        if after(inner.meter.last_tx(), stall) <= Instant::now() {
                            inner.close(RpcError::new(
                                ErrorCode::Timeout,
                                "write stalled: the peer does not read",
                            ));
                            break None;
                        }
                    }
                }
            }
        };
        drop(permit);
        match sent {
            None => break,
            Some(Ok(())) => {}
            Some(Err(e)) => {
                inner.close(codec::unavailable(format_args!("write failed: {e}")));
                // Сломанный сокет второй записи не переживёт: хвост не шлём.
                return;
            }
        }
    }

    // Хвост управляющих кадров (`stream_end` и `close` с причиной).
    let flush = async {
        while let Ok(Queued { raw, .. }) = ctrl.try_recv() {
            if tx.feed(Outbound::Frame(raw)).await.is_err() {
                return;
            }
        }
        _ = tx.send(Outbound::Close(inner.reason.get().cloned())).await;
        _ = tx.close().await;
    };
    _ = tokio::time::timeout(FLUSH_ON_CLOSE, flush).await;
}

async fn reader<S: Service>(inner: Arc<Inner>, mut rx: Rx, service: Arc<S>) {
    let notify = notifier(inner.clone(), service.clone());

    let reason = loop {
        let next = tokio::select! {
            biased;
            () = inner.closed.cancelled() => break None,
            next = rx.next() => next,
        };

        let raw = match next {
            None => break Some(codec::unavailable("connection closed by peer")),
            Some(Err(TransportError::TooLarge)) => {
                break Some(RpcError::invalid_params("frame exceeds max_frame"));
            }
            Some(Err(e)) => break Some(codec::unavailable(e)),
            Some(Ok(raw)) => raw,
        };
        inner.meter.touch_rx();

        let frame = match codec::decode_frame(&raw) {
            Ok(frame) => frame,
            Err(e) => {
                break Some(RpcError::invalid_params(format_args!(
                    "malformed frame: {e}"
                )));
            }
        };
        on_frame(&inner, &service, &notify, frame);
    };

    if let Some(reason) = reason {
        inner.close(reason);
    }
}

fn on_frame<S: Service>(
    inner: &Arc<Inner>,
    service: &Arc<S>,
    notify: &mpsc::Sender<(String, Payload)>,
    frame: Frame,
) {
    match frame {
        Frame::Request(request) => inner.serve(service, request),
        Frame::Response(response) => inner.on_response(response),
        Frame::Notify(NotifyFrame { method, params }) if method == method::CLOSE => {
            let reason = codec::decode::<RpcError>(&params)
                .unwrap_or_else(|e| codec::unavailable(format_args!("closed by peer: {e}")));
            inner.shut(reason, false);
        }
        Frame::Notify(NotifyFrame { method, params }) => {
            if method != method::PING && notify.try_send((method, params)).is_err() {
                log::warn!(remote = inner.remote.as_str(); "notification dropped: handler is behind");
            }
        }
        Frame::StreamItem(item) => inner.on_item(item),
        Frame::StreamHeartbeat(StreamHeartbeat { id }) => inner.on_heartbeat(id),
        Frame::StreamEnd(end) => inner.on_end(end),
        Frame::Cancel(Cancel { id }) => inner.on_cancel(id),
        Frame::Unknown => log::debug!(remote = inner.remote.as_str(); "unknown frame kind skipped"),
    }
}

/// Уведомления обрабатываются по одному: их порядок — порядок публикации.
fn notifier<S: Service>(inner: Arc<Inner>, service: Arc<S>) -> mpsc::Sender<(String, Payload)> {
    let (tx, mut rx) = mpsc::channel::<(String, Payload)>(inner.cfg.stream_buffer());
    tokio::spawn(async move {
        loop {
            let next = tokio::select! {
                biased;
                () = inner.closed.cancelled() => break,
                next = rx.recv() => next,
            };
            let Some((method, params)) = next else {
                break;
            };
            if AssertUnwindSafe(service.notify(method, params))
                .catch_unwind()
                .await
                .is_err()
            {
                log::error!(remote = inner.remote.as_str(); "notification handler panicked");
            }
        }
    });
    tx
}

/// Тишина считается по байтам сокета: крупный кадр на медленной линии
/// идёт дольше `ping_timeout`, но соединение живо.
async fn pinger(inner: Arc<Inner>) {
    let silence = *inner.cfg.ping_timeout;
    let ping = match codec::encode(&Ping {}) {
        Ok(ping) => ping,
        Err(e) => {
            inner.close(RpcError::new(ErrorCode::Internal, e));
            return;
        }
    };

    let mut tick = ticker(*inner.cfg.ping_interval);

    loop {
        let deadline = after(inner.meter.last_rx(), silence);
        tokio::select! {
            () = inner.closed.cancelled() => break,
            () = tokio::time::sleep_until(deadline) => {
                if after(inner.meter.last_rx(), silence) <= Instant::now() {
                    inner.close(RpcError::new(ErrorCode::Timeout, "ping timeout"));
                    break;
                }
            }
            _ = tick.tick() => inner.send_ctrl(&Frame::Notify(NotifyFrame {
                method: method::PING.to_owned(),
                params: ping.clone(),
            })),
        }
    }
}
