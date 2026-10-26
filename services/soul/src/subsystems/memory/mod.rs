//! Компонент `memory` (`03`): salience, видимость, recall, hot-блок,
//! дедуп и тулы, эмбеддер с воркером backfill. Авторизация операций
//! `memory.*` — `Policy`; гейты тулов — здесь (`03 §7`).

use std::{collections::BTreeMap, pin::pin, time::Duration};

use eva::supervisor::SlaveRx;
use eva_protocol::{
    errors::AuxResult,
    types::{memory::Kind, turn::TurnView},
    wire::events::Event,
};
use tokio::time::MissedTickBehavior;

use crate::{
    components::Components,
    db::{Db as _, DbError, Queries as _, memory::Scope, stats::Metric},
    shared::Shared,
    subsystems::memory::{embed::Embedder, visibility::Gate},
};

pub mod changes;
pub mod embed;
pub mod hot;
pub mod recall;
pub mod salience;
pub mod text;
pub mod tools;
pub mod visibility;

/// Воркер backfill просыпается не реже этого; события памяти будят его раньше.
pub const BACKFILL_EVERY: Duration = Duration::from_secs(60);

/// Hot-блок чата на старте сегмента (`03 §5`): видимость уровня чата,
/// тема — эмбеддинг последних реплик с таймаутом, слабое подкрепление
/// включённых.
///
/// `turns::ports::MemoryPort` сохраняет блок в снимке сегмента вместе с его id.
pub async fn hot_block<C: Components>(
    shared: &Shared<C>,
    view: &TurnView,
    ratio: Option<f64>,
) -> AuxResult<hot::Block> {
    let cfg = &shared.cfg().memory;
    let now = shared.now();
    let gate = Gate::hot(view);
    let reader = shared.db().reader().await?;
    let rows = reader.run(|c| c.memory().current()).await?;
    let withheld = rows.iter().any(|r| !gate.allows(r.visibility));
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|r| gate.allows(r.visibility))
        .collect();

    let mut topic = BTreeMap::new();
    if let Some(embedder) = Embedder::resolve(shared) {
        let (chat, n) = (view.chat, cfg.hot.topic_replies);
        let replies = reader.run(move |c| c.chats().last_speech(chat, n)).await?;
        if !replies.is_empty() {
            let model = embedder.model().clone();
            let scope = Scope {
                kind: Some(Kind::Event),
                ..Scope::default()
            };
            let vectors = reader
                .run(move |c| c.memory().vectors(&scope, &model))
                .await?;
            if let Some(query) = embedder
                .query(&replies.join("\n"), *cfg.hot.topic_timeout)
                .await
            {
                for (id, v) in &vectors {
                    topic.insert(*id, text::cosine(&query, v));
                }
            }
        }
    }

    drop(reader);
    let block = hot::build(
        &hot::Input {
            rows: &rows,
            now,
            zone: &shared.day_zone(),
            ratio,
            topic: &topic,
            withheld,
        },
        cfg,
    );
    if !block.ids.is_empty() {
        let (ids, delta) = (block.ids.clone(), cfg.reinforce.passive);
        shared
            .db()
            .write(move |tx| tx.memory().reinforce(&ids, delta, None))
            .await?;
    }
    for (kind, _) in block.dropped.kinds() {
        shared
            .bump(Metric::MemoryHotDropped, &kind.to_string())
            .await;
    }
    shared.gauges().set_hot_tokens(block.tokens);
    if block.dropped.core > 0 {
        core_overflow(shared, view.chat, block.dropped.core).await?;
    }
    Ok(block)
}

async fn core_overflow<C: Components>(
    shared: &Shared<C>,
    chat: eva_protocol::types::ChatId,
    count: u32,
) -> AuxResult<()> {
    use crate::db::{DbResult, meta::CORE_OVERFLOW_NOTIFIED_AT, stats::elapsed};

    let now = shared.now();
    let interval = *shared.cfg().memory.hot.core_overflow_notify;
    let claimed = shared
        .db()
        .write(move |tx| {
            if tx
                .meta()
                .timestamp(CORE_OVERFLOW_NOTIFIED_AT)?
                .is_some_and(|last| elapsed(last, now) < interval)
            {
                return DbResult::Ok(false);
            }
            tx.meta().set_timestamp(CORE_OVERFLOW_NOTIFIED_AT, now)?;
            Ok(true)
        })
        .await?;
    if claimed {
        let mut session = crate::session::internal(shared);
        if let Err(error) = session . chats () . notify ("Core over budget" . into () , format ! ("{count} core memory records did not fit. Fold related core records, remove obsolete records, or demote them to facts.") , Some (chat)) . await {
            log::warn!(error:% = error; "core overflow notification failed");
        }
    }
    Ok(())
}

/// Строк памяти без вектора активной модели (gauge `memory.embed_pending`, `09 §4`).
pub async fn embed_pending<C: Components>(shared: &Shared<C>) -> Result<u32, DbError> {
    let Some(embedder) = Embedder::resolve(shared) else {
        return Ok(0);
    };
    let (model, local) = (embedder.model().clone(), embedder.local());
    shared
        .db()
        .reader()
        .await?
        .run(move |c| c.memory().embed_pending(&model, local))
        .await
}

async fn backfill<C: Components>(shared: &Shared<C>) {
    let Some(embedder) = Embedder::resolve(shared) else {
        return;
    };
    match embedder.backfill().await {
        Ok(0) => {}
        Ok(n) => log::info!(rows = n, model:% = embedder.model(); "memory embeddings backfilled"),
        Err(e) => {
            log::warn!(error:% = e, model:% = embedder.model(); "memory embedding backfill failed")
        }
    }
}

/// Компонент `Supervisor`: backfill эмбеддингов на старте, по событиям
/// памяти и раз в `BACKFILL_EVERY`, до `Shutdown`.
pub async fn run<C: Components>(shared: Shared<C>, mut rx: SlaveRx) -> eyre::Result<()> {
    let mut events = shared.bus().subscribe();
    let mut tick = tokio::time::interval(BACKFILL_EVERY);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut stop = pin!(rx.recv());
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = tick.tick() => backfill(&shared).await,
            event = events.recv() => match event {
                Ok(published) if matches!(published.event, Event::MemoryChanged(_)) => backfill(&shared).await,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    log::info!("memory stopped");
    Ok(())
}
