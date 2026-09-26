//! Симметричное мультиплексированное соединение (`06 §1`).
//!
//! Оба пира шлют запросы, потоки и отмены по одному сокету. Читатель сокета
//! никого не ждёт: у каждого потока свой буфер `stream_buffer`, и его
//! переполнение рвёт только этот поток (`quota` получателю, `cancel`
//! отправителю) — иначе один медленный подписчик задержал бы `ping` и уронил
//! соединение по `ping_timeout`.
//!
//! Нумерация: `stream_item`/`stream_end` не несут направления, а идут и по
//! потокам, открытым получателем, и по загрузкам пира. Поэтому принимающая
//! сторона (`IdSeq::server`) нумерует свои запросы с `2^63`, и пространства
//! id двух пиров не пересекаются.

use std::{mem, pin::Pin, sync::Arc, time::Duration};

use futures::{Stream, StreamExt as _};
use tokio::{
    sync::{Notify, oneshot},
    time::Instant,
};

use eva_core::ByteStream;
use eva_protocol::{
    types::Bytes,
    wire::{
        RpcError,
        frame::{Frame, Notify as NotifyFrame, Payload, Request, StreamEnd, StreamItem},
    },
};

use crate::{codec, config::Config, transport::Conn};

use self::inner::{Feed, Inner, Pending};

pub use self::remote::RemoteStream;

mod body;
mod inner;
mod remote;

/// Счётчик исходящих id. После `hello.resume` SDK продолжает прежний
/// счётчик: иначе новый запрос совпал бы с id из окна идемпотентности.
#[derive(Debug, Clone)]
pub struct IdSeq(Arc<std::sync::atomic::AtomicU64>);

impl IdSeq {
    /// Инициатор соединения (модуль): с нуля.
    pub fn new() -> Self {
        Self::starting_at(0)
    }

    /// Принявший соединение (ядро): верхняя половина пространства id.
    pub fn server() -> Self {
        Self::starting_at(1 << 63)
    }

    pub fn starting_at(first: u64) -> Self {
        Self(Arc::new(std::sync::atomic::AtomicU64::new(first)))
    }

    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for IdSeq {
    fn default() -> Self {
        Self::new()
    }
}

/// Как на запрос отвечают.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Один `response`.
    Unary,
    /// `stream_item`* и `stream_end`.
    Stream,
    /// Чанки от вызывающего, затем `response`.
    Upload,
}

/// Входящий запрос.
#[derive(Debug)]
pub struct Call {
    pub id: u64,
    pub method: String,
    pub params: Payload,
}

/// Элементы потока, который отдаёт обработчик; `Err` — последний элемент.
pub type Outgoing = Pin<Box<dyn Stream<Item = Result<Payload, RpcError>> + Send>>;

/// Обработчик входящих запросов пира: диспетчер операций у ядра, методы
/// ядро → модуль у SDK.
pub trait Service: Send + Sync + 'static {
    /// `None` — `unknown_method`.
    fn kind(&self, method: &str) -> Option<Kind>;

    /// `true` — `cancel` не обрывает не-потоковый обработчик: пир сразу
    /// получает `cancelled`, а обработчик доживает, потому что его эффект
    /// мог уже лечь и итог нужен окну идемпотентности.
    fn survives_cancel(&self, method: &str) -> bool {
        _ = method;
        false
    }

    fn unary(&self, call: Call) -> impl Future<Output = Result<Payload, RpcError>> + Send;

    fn stream(&self, call: Call) -> impl Future<Output = Result<Outgoing, RpcError>> + Send {
        async move { Err(RpcError::unknown_method(&call.method)) }
    }

    fn upload(
        &self,
        call: Call,
        body: ByteStream,
    ) -> impl Future<Output = Result<Payload, RpcError>> + Send {
        drop(body);
        async move { Err(RpcError::unknown_method(&call.method)) }
    }

    /// Уведомления приходят по одному, в порядке отправки; `ping` сюда не попадает.
    fn notify(&self, method: String, params: Payload) -> impl Future<Output = ()> + Send {
        _ = (method, params);
        async {}
    }
}

/// Ручка соединения. Клоны делят одно соединение; drop ручки его не
/// закрывает — закрывает `close`, пир или транспорт.
#[derive(Clone)]
pub struct Peer(Arc<Inner>);

impl std::fmt::Debug for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Peer")
            .field("remote", &self.0.remote)
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl Peer {
    /// Запускает чтение, запись и ping поверх уже прошедшего handshake соединения.
    pub fn spawn<S: Service>(conn: Conn, cfg: Config, ids: IdSeq, service: S) -> Self {
        Self(Inner::spawn(conn, cfg, ids, Arc::new(service)))
    }

    pub fn remote(&self) -> &str {
        &self.0.remote
    }

    pub fn config(&self) -> &Config {
        &self.0.cfg
    }

    pub fn ids(&self) -> &IdSeq {
        &self.0.ids
    }

    /// Не-потоковый вызов. `request_timeout` — срок тишины: heartbeat пира
    /// на id запроса продлевает его, так что долгий живой ответ дожидается.
    pub async fn call(&self, method: &str, params: Payload) -> Result<Payload, RpcError> {
        let (mut rx, alive, mut guard) = self.request(method, params).await?;
        match self
            .reply(&mut rx, &alive, *self.0.cfg.request_timeout)
            .await
        {
            Some(result) => {
                guard.disarm();
                result
            }
            None => Err(RpcError::timeout()),
        }
    }

    /// Жёсткий срок `limit` вне зависимости от heartbeat; истёк — `cancel`
    /// пиру и `timeout` вызывающему.
    pub async fn call_timeout(
        &self,
        method: &str,
        params: Payload,
        limit: Duration,
    ) -> Result<Payload, RpcError> {
        let (rx, _alive, mut guard) = self.request(method, params).await?;
        match tokio::time::timeout(limit, rx).await {
            Ok(result) => {
                guard.disarm();
                result.unwrap_or_else(|_| Err(self.0.reason()))
            }
            Err(_) => Err(RpcError::timeout()),
        }
    }

    async fn request(&self, method: &str, params: Payload) -> Result<Reply, RpcError> {
        let id = self.0.ids.next();
        let (tx, rx) = oneshot::channel();
        let alive = Arc::new(Notify::new());
        let mut guard = self.0.register(
            id,
            Pending::Unary {
                tx,
                alive: alive.clone(),
            },
        )?;

        let request = Frame::Request(Request {
            id,
            method: method.to_owned(),
            params,
        });
        if let Err(e) = self.0.send_call(&request).await {
            guard.abandon();
            return Err(e);
        }
        Ok((rx, alive, guard))
    }

    /// Ответ либо `None`, если пир молчал дольше `idle`.
    async fn reply(
        &self,
        rx: &mut oneshot::Receiver<Result<Payload, RpcError>>,
        alive: &Notify,
        idle: Duration,
    ) -> Option<Result<Payload, RpcError>> {
        loop {
            tokio::select! {
                biased;
                result = &mut *rx => return Some(result.unwrap_or_else(|_| Err(self.0.reason()))),
                () = alive.notified() => {}
                () = tokio::time::sleep(idle) => return None,
            }
        }
    }

    /// Открывает поток к себе. Ждёт первый кадр потока: отказ до первого
    /// элемента — `Err` отсюда, а не элемент.
    pub async fn open(&self, method: &str, params: Payload) -> Result<RemoteStream, RpcError> {
        let id = self.0.ids.next();
        let (feed, rx, end) = Feed::new(self.0.cfg.stream_buffer());
        let guard = self.0.register(id, Pending::Stream(feed))?;
        let mut stream = RemoteStream::new(id, rx, end, guard, *self.0.cfg.stream_idle_timeout);

        let request = Frame::Request(Request {
            id,
            method: method.to_owned(),
            params,
        });
        if let Err(e) = self.0.send_call(&request).await {
            stream.abandon();
            return Err(e);
        }

        match tokio::time::timeout(*self.0.cfg.request_timeout, stream.first()).await {
            Ok(Ok(())) => Ok(stream),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(RpcError::timeout()),
        }
    }

    /// Загрузка: запрос, чанки ровно по `blob_chunk` (последний — остаток;
    /// `stream_item` с msgpack `Bytes`), `stream_end`, затем `response`. Мелкие
    /// куски тела склеиваются: буфер получателя считает кадры, а не байты.
    /// Ранний ответ пира прерывает отправку тела.
    pub async fn upload(
        &self,
        method: &str,
        params: Payload,
        mut body: ByteStream,
    ) -> Result<Payload, RpcError> {
        let id = self.0.ids.next();
        let (tx, mut rx) = oneshot::channel();
        let alive = Arc::new(Notify::new());
        let mut guard = self.0.register(
            id,
            Pending::Unary {
                tx,
                alive: alive.clone(),
            },
        )?;

        // Тело идёт той же полосой, что и запрос: чанк не обгонит запрос.
        let request = Frame::Request(Request {
            id,
            method: method.to_owned(),
            params,
        });
        if let Err(e) = self.0.send_bulk(&request).await {
            guard.abandon();
            return Err(e);
        }

        let mut chunks = Chunks::new(self, id);
        // Медленный источник не держит неполный чанк дольше `ping_interval`,
        // иначе получатель закрыл бы тело по `stream_idle_timeout`.
        let mut flush_at: Option<Instant> = None;
        loop {
            let next = tokio::select! {
                biased;
                early = &mut rx => {
                    guard.disarm();
                    return early.unwrap_or_else(|_| Err(self.0.reason()));
                }
                next = body.next() => next,
                () = tokio::time::sleep_until(flush_at.unwrap_or_else(Instant::now)), if flush_at.is_some() => {
                    chunks.flush().await?;
                    flush_at = None;
                    continue;
                }
            };

            match next {
                Some(Ok(bytes)) => {
                    chunks.push(&bytes).await?;
                    flush_at = match (chunks.is_empty(), flush_at) {
                        (true, _) => None,
                        (false, None) => Some(Instant::now() + *self.0.cfg.ping_interval),
                        (false, at) => at,
                    };
                }
                Some(Err(aux)) => {
                    let error = codec::aux(&aux);
                    _ = self
                        .0
                        .send_bulk(&Frame::StreamEnd(StreamEnd {
                            id,
                            error: Some(error.clone()),
                        }))
                        .await;
                    return Err(error);
                }
                None => break,
            }
        }

        chunks.flush().await?;
        self.0
            .send_bulk(&Frame::StreamEnd(StreamEnd { id, error: None }))
            .await?;

        match self
            .reply(&mut rx, &alive, *self.0.cfg.request_timeout)
            .await
        {
            Some(result) => {
                guard.disarm();
                result
            }
            None => Err(RpcError::timeout()),
        }
    }

    /// Уведомление без ответа; порядок уведомлений сохраняется.
    pub async fn notify(&self, method: &str, params: Payload) -> Result<(), RpcError> {
        self.0
            .send_call(&Frame::Notify(NotifyFrame {
                method: method.to_owned(),
                params,
            }))
            .await
    }

    /// Закрывает соединение: обслуживаемые потоки получают `stream_end{reason}`,
    /// свои незавершённые вызовы — `reason`.
    pub fn close(&self, reason: RpcError) {
        self.0.close(reason);
    }

    pub fn is_closed(&self) -> bool {
        self.0.is_closed()
    }

    /// Причина закрытия, если соединение закрыто.
    pub fn close_reason(&self) -> Option<RpcError> {
        self.is_closed().then(|| self.0.reason())
    }

    /// Ждёт закрытия и отдаёт причину.
    pub async fn closed(&self) -> RpcError {
        self.0.closed.cancelled().await;
        self.0.reason()
    }
}

/// Ожидание ответа своего вызова: канал ответа, heartbeat пира, регистрация.
type Reply = (
    oneshot::Receiver<Result<Payload, RpcError>>,
    Arc<Notify>,
    inner::Guard,
);

/// Тело загрузки, нарезанное на чанки ровно по `blob_chunk`.
struct Chunks<'a> {
    peer: &'a Peer,
    id: u64,
    size: usize,
    seq: u64,
    buf: Vec<u8>,
}

impl<'a> Chunks<'a> {
    fn new(peer: &'a Peer, id: u64) -> Self {
        Self {
            peer,
            id,
            size: peer.0.cfg.blob_chunk(),
            seq: 0,
            buf: Vec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    async fn push(&mut self, mut bytes: &[u8]) -> Result<(), RpcError> {
        while !bytes.is_empty() {
            if self.buf.is_empty() && bytes.len() >= self.size {
                let (piece, rest) = bytes.split_at(self.size);
                self.send(piece).await?;
                bytes = rest;
                continue;
            }
            let take = (self.size - self.buf.len()).min(bytes.len());
            let (piece, rest) = bytes.split_at(take);
            self.buf.extend_from_slice(piece);
            bytes = rest;
            if self.buf.len() == self.size {
                self.flush().await?;
            }
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), RpcError> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let buf = mem::take(&mut self.buf);
        self.send(&buf).await?;
        self.buf = buf;
        self.buf.clear();
        Ok(())
    }

    async fn send(&mut self, piece: &[u8]) -> Result<(), RpcError> {
        let item = codec::encode(&Bytes::from(piece))
            .map_err(|e| RpcError::new(eva_protocol::wire::ErrorCode::Internal, e))?;
        self.peer
            .0
            .send_bulk(&Frame::StreamItem(StreamItem {
                id: self.id,
                seq: self.seq,
                item,
            }))
            .await?;
        self.seq += 1;
        Ok(())
    }
}
