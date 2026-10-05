//! Живые туры (`07 §8`): не больше одного на чат, ворота append'а реплик
//! против закрытия тура, лента и последний запрос чата.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use eva::time::Timestamp;
use tokio::sync::{Notify, oneshot};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use eva_protocol::types::{
    ChatId, Effort, ModelId, PendingId, SegmentId, Thinking, TurnId, generate::GenerateRequest,
};

use crate::subsystems::turns::{cache::Ttl, ctx::Ctx, feed::Feed, log::EndedBy, pending::Answered};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Ручка живого тура.
#[derive(Debug, Clone)]
pub struct LiveTurn {
    pub turn: TurnId,
    pub cancel: CancellationToken,
    pub closed: CancellationToken,
    /// Источник отмены: первый вызвавший побеждает.
    pub by: Arc<OnceLock<EndedBy>>,
    /// Реплика в живой тур: шаг, ждущий pending или провайдера, просыпается.
    pub wake: Arc<Notify>,
}

impl LiveTurn {
    fn new(turn: TurnId) -> Self {
        Self {
            turn,
            cancel: CancellationToken::new(),
            closed: CancellationToken::new(),
            by: Arc::default(),
            wake: Arc::default(),
        }
    }

    pub fn cancel(&self, by: EndedBy) {
        _ = self.by.set(by);
        self.cancel.cancel();
    }
}

/// Последний запрос тура чата: тёплая компакция, keep-alive и разрывы кэша.
#[derive(Debug, Clone)]
pub struct LastRequest {
    pub request: GenerateRequest,
    pub model: ModelId,
    pub effort: Effort,
    pub thinking: Thinking,
    pub segment: SegmentId,
    pub tools: Vec<String>,
    pub at: Timestamp,
    /// `input + cache_read + cache_write` ответа.
    pub prompt: u64,
    pub context_edits: u32,
    pub ttl: Ttl,
}

/// Чат в реестре.
pub struct ChatSlot {
    pub chat: ChatId,
    /// Append реплики и закрытие тура не пересекаются: реплика либо в живом
    /// туре до его drain-проверки, либо стартует новый.
    pub gate: Arc<tokio::sync::Mutex<()>>,
    ingest_pauses: Arc<AtomicUsize>,
    live: Mutex<Option<LiveTurn>>,
    pub feed: Feed,
    keep_alive: Mutex<Option<CancellationToken>>,
    last: Mutex<Option<LastRequest>>,
    ctx: Mutex<Option<Ctx>>,
}

impl ChatSlot {
    fn new(chat: ChatId, capacity: usize) -> Self {
        Self {
            chat,
            gate: Arc::default(),
            ingest_pauses: Arc::default(),
            live: Mutex::new(None),
            feed: Feed::new(capacity),
            keep_alive: Mutex::new(None),
            last: Mutex::new(None),
            ctx: Mutex::new(None),
        }
    }

    pub fn live(&self) -> Option<LiveTurn> {
        lock(&self.live).clone()
    }

    /// Закрытие тура берёт gate, поэтому пауза ввода живёт отдельно от блокировки.
    pub async fn pause_ingest(&self) -> IngestPause {
        let _gate = self.gate.lock().await;
        self.ingest_pauses.fetch_add(1, Ordering::AcqRel);
        IngestPause(self.ingest_pauses.clone())
    }

    pub fn ingest_paused(&self) -> bool {
        self.ingest_pauses.load(Ordering::Acquire) > 0
    }

    /// Только под `gate`.
    pub fn set_live(&self, live: Option<LiveTurn>) {
        *lock(&self.live) = live;
    }

    pub fn last_request(&self) -> Option<LastRequest> {
        lock(&self.last).clone()
    }

    pub fn set_last_request(&self, last: LastRequest) {
        *lock(&self.last) = Some(last);
    }

    /// Контекст последнего тура чата: `tools.catalog{chat}` считает по нему.
    pub fn last_ctx(&self) -> Option<Ctx> {
        lock(&self.ctx).clone()
    }

    pub fn set_ctx(&self, ctx: Ctx) {
        *lock(&self.ctx) = Some(ctx);
    }

    /// Новый keep-alive вытесняет прежний; `None` — только отмена.
    pub fn keep_alive(&self, token: Option<CancellationToken>) {
        let old = std::mem::replace(&mut *lock(&self.keep_alive), token);
        if let Some(old) = old {
            old.cancel();
        }
    }
}

pub struct IngestPause(Arc<AtomicUsize>);

impl Drop for IngestPause {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Реестр туров ядра.
pub struct Turns {
    chats: Mutex<BTreeMap<ChatId, Arc<ChatSlot>>>,
    tasks: TaskTracker,
    waiters: Mutex<BTreeMap<PendingId, oneshot::Sender<Answered>>>,
    capacity: usize,
    persona: String,
    scrubber: super::secrets::Scrubber,
}

impl std::fmt::Debug for Turns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Turns").finish_non_exhaustive()
    }
}

impl Turns {
    /// `persona` — байты `context.persona_file`; `capacity` — буфер ленты.
    pub fn new(persona: String, capacity: usize, scrubber: super::secrets::Scrubber) -> Self {
        Self {
            chats: Mutex::default(),
            tasks: TaskTracker::new(),
            waiters: Mutex::default(),
            capacity,
            persona,
            scrubber,
        }
    }

    pub fn persona(&self) -> &str {
        &self.persona
    }

    pub fn scrubber(&self) -> &super::secrets::Scrubber {
        &self.scrubber
    }

    pub fn slot(&self, chat: ChatId) -> Arc<ChatSlot> {
        lock(&self.chats)
            .entry(chat)
            .or_insert_with(|| Arc::new(ChatSlot::new(chat, self.capacity)))
            .clone()
    }

    pub fn get(&self, chat: ChatId) -> Option<Arc<ChatSlot>> {
        lock(&self.chats).get(&chat).cloned()
    }

    /// Живой тур чата.
    pub fn live(&self, chat: ChatId) -> Option<TurnId> {
        self.get(chat).and_then(|s| s.live()).map(|l| l.turn)
    }

    /// Чаты с живым туром: `chats.list{live}` и `Chat.live_turn`.
    pub fn live_chats(&self) -> BTreeMap<ChatId, TurnId> {
        lock(&self.chats)
            .iter()
            .filter_map(|(chat, slot)| slot.live().map(|l| (*chat, l.turn)))
            .collect()
    }

    /// Кооперативная отмена (`07 §8`); `false` — живого тура нет.
    pub fn cancel(&self, chat: ChatId, by: EndedBy) -> bool {
        match self.get(chat).and_then(|s| s.live()) {
            Some(live) => {
                live.cancel(by);
                true
            }
            None => false,
        }
    }

    /// Новый тур под воротами чата.
    pub fn claim(&self, slot: &ChatSlot, turn: TurnId) -> LiveTurn {
        let live = LiveTurn::new(turn);
        slot.set_live(Some(live.clone()));
        live
    }

    pub fn spawn<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.tasks.spawn(task);
    }

    /// Остановка ядра: все туры отменяются и закрываются до закрытия БД.
    pub async fn shutdown(&self) {
        let slots: Vec<Arc<ChatSlot>> = lock(&self.chats).values().cloned().collect();
        for slot in slots {
            if let Some(live) = slot.live() {
                live.cancel(EndedBy::Shutdown);
            }
            slot.keep_alive(None);
        }
        self.tasks.close();
        self.tasks.wait().await;
    }

    pub fn wait_answer(&self, pending: PendingId) -> oneshot::Receiver<Answered> {
        let (tx, rx) = oneshot::channel();
        lock(&self.waiters).insert(pending, tx);
        rx
    }

    /// `false` — вызов уже не ждёт: ответ ушёл в лог (`late_answer`).
    pub fn answer(&self, pending: &PendingId, answer: Answered) -> bool {
        match lock(&self.waiters).remove(pending) {
            Some(tx) => tx.send(answer).is_ok(),
            None => false,
        }
    }

    pub fn forget_waiter(&self, pending: &PendingId) {
        lock(&self.waiters).remove(pending);
    }
}
