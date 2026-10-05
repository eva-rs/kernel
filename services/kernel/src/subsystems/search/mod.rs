//! Поиск по чатам (`05 §7`): индексатор по `indexed_at` и гибридный
//! запрос. Операция `chats.search` — `handling::chats::Search`.

use std::{pin::pin, time::Duration};

use eva::supervisor::SlaveRx;
use eva_protocol::wire::events::Event;
use tokio::time::{Instant, MissedTickBehavior};

use crate::{components::Components, shared::Shared, subsystems::memory::embed::Embedder};

pub mod index;
pub mod query;

/// Реплик за одну пачку индексации.
pub const BATCH: u32 = 256;
/// Страховочный проход ловит речь без тура (silent-append).
pub const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// Всё с `indexed_at IS NULL`, затем векторы чанков, если есть эмбеддер.
pub async fn sweep<C: Components>(shared: &Shared<C>) {
    loop {
        match index::index_batch(shared, BATCH).await {
            Ok(done) if done.messages == 0 && done.titles == 0 => break,
            Ok(done) => {
                log::debug!(messages = done.messages, chunks = done.chunks, titles = done.titles; "chat speech indexed");
                if done.messages < BATCH as usize {
                    break;
                }
            }
            Err(e) => {
                log::warn!(error:% = e; "chat speech indexing failed");
                break;
            }
        }
    }
    if let Some(embedder) = Embedder::resolve(shared) {
        match index::embed_chunks(shared, &embedder).await {
            Ok(0) => {}
            Ok(n) => log::debug!(chunks = n, model:% = embedder.model(); "chat speech embedded"),
            Err(e) => log::warn!(error:% = e; "chat speech embedding failed"),
        }
    }
}

/// События, после которых в чате появилась речь или название для индекса.
const fn wakes(event: &Event) -> bool {
    matches!(
        event,
        Event::TurnFinished(_) | Event::ChatCreated(_) | Event::ChatRestored(_)
    )
}

/// Компонент `Supervisor`: событие взводит таймер `chats.search.index_delay`
/// (пачка вместо прохода на каждую реплику), страховка — раз в `SWEEP_EVERY`;
/// до `Shutdown`.
pub async fn run<C: Components>(shared: Shared<C>, mut rx: SlaveRx) -> eyre::Result<()> {
    let delay = *shared.cfg().chats.search.index_delay;
    let mut events = shared.bus().subscribe();
    let mut tick = tokio::time::interval(SWEEP_EVERY);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut armed: Option<Instant> = None;
    let mut stop = pin!(rx.recv());
    loop {
        let timer = async {
            match armed {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = &mut stop => break,
            _ = tick.tick() => sweep(&shared).await,
            () = timer => {
                armed = None;
                sweep(&shared).await;
            }
            event = events.recv() => match event {
                Ok(published) if wakes(&published.event) => {
                    armed.get_or_insert_with(|| Instant::now() + delay);
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    log::info!("search indexer stopped");
    Ok(())
}
