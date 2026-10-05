//! Лента items чата (`07 §7`): живые события тура и items событий лога в
//! один broadcast на чат. `seq` item'а из лога — `events.id` источника,
//! живого — последний записанный `events.id` чата; `sub` — порядок внутри.

use std::sync::{
    Mutex,
    atomic::{AtomicI64, Ordering},
};

use tokio::sync::broadcast;

use eva_protocol::types::{
    ChatId, EventId, TurnId,
    turn::{FeedEvent, Item, ItemBody, ItemStatus, TurnItem},
};

/// Сообщение broadcast'а: `origin` решает, что `attach` уже отдал историей.
#[derive(Debug, Clone)]
pub struct FeedMsg {
    pub item: TurnItem,
    pub origin: Origin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Из события лога: в истории оно уже есть, если `id ≤` отданного.
    Derived(EventId),
    /// Только живое: дельты, `usage`, `turn.done`.
    LiveOnly,
}

/// Лента одного чата.
pub struct Feed {
    tx: broadcast::Sender<FeedMsg>,
    /// Последний записанный `events.id` чата.
    last: AtomicI64,
    /// `sub` живых item'ов внутри одного `seq`.
    live_sub: Mutex<(i64, u32)>,
}

impl Feed {
    pub fn new(capacity: usize) -> Self {
        Self {
            tx: broadcast::Sender::new(capacity.max(1)),
            last: AtomicI64::new(0),
            live_sub: Mutex::new((0, 0)),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<FeedMsg> {
        self.tx.subscribe()
    }

    /// Событие записано: следующие живые item'ы несут его `seq`.
    pub fn note(&self, event: EventId) {
        self.last.fetch_max(event.0, Ordering::AcqRel);
    }

    pub fn last(&self) -> EventId {
        EventId(self.last.load(Ordering::Acquire))
    }

    /// Item события лога.
    pub fn derived(&self, event: EventId, sub: u32, body: FeedEvent) {
        self.note(event);
        self.send(event, sub, body, Origin::Derived(event));
    }

    /// Живое без события.
    pub fn live(&self, body: FeedEvent) {
        let seq = self.last();
        let sub = {
            let mut cur = self.live_sub.lock().unwrap_or_else(|e| e.into_inner());
            if cur.0 != seq.0 {
                *cur = (seq.0, 0);
            }
            cur.1 += 1;
            cur.1
        };
        self.send(seq, sub, body, Origin::LiveOnly);
    }

    fn send(&self, seq: EventId, sub: u32, event: FeedEvent, origin: Origin) {
        // Без подписчиков лента теряется: история восстановит её из лога.
        _ = self.tx.send(FeedMsg {
            item: TurnItem {
                seq,
                sub,
                replayed: false,
                event,
            },
            origin,
        });
    }
}

/// Id item'а из лога: `<event_id>:<n>`.
pub fn logged_id(event: EventId, n: usize) -> String {
    format!("{}:{n}", event.0)
}

/// Живой id ответа шага: `<turn_id>:<step>:a` / `:t`.
pub fn live_id(turn: &TurnId, step: u32, thinking: bool) -> String {
    format!("{turn}:{step}:{}", if thinking { 't' } else { 'a' })
}

/// Id плана чата.
pub fn plan_id(chat: ChatId) -> String {
    format!("plan:{}", chat.0)
}

pub fn item(
    id: String,
    turn: Option<&TurnId>,
    seq: EventId,
    status: ItemStatus,
    body: ItemBody,
) -> Item {
    Item {
        id,
        turn: turn.cloned(),
        seq,
        status,
        body,
    }
}
