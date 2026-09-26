//! Внутренняя шина событий (`00 §4`, `06 §7`). Заготовка M1: рассылку
//! модулям с фильтром `Policy::events().deliver` строит подсистема `modules`.

use eva::data;
use tokio::sync::broadcast;

use eva_protocol::wire::events::Event;

use crate::db::surfaces::SurfaceRef;

/// Событие и surface, по которому решается доставка; `None` — только клиентам.
#[data]
pub struct Published {
    pub event: Event,
    pub surface: Option<SurfaceRef>,
}

pub struct Bus(broadcast::Sender<Published>);

impl Bus {
    pub fn new(capacity: usize) -> Self {
        Self(broadcast::Sender::new(capacity))
    }

    /// Без подписчиков событие теряется: реплея у шины нет (`06 §7`).
    pub fn publish(&self, event: Event, surface: Option<SurfaceRef>) {
        _ = self.0.send(Published { event, surface });
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Published> {
        self.0.subscribe()
    }
}
