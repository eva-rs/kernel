//! Продление кэша последнего запроса до следующего тура или лимита времени.

use crate::{
    components::Components,
    db::stats::Op,
    shared::Shared,
    subsystems::{
        stats::{Call, Recorder},
        turns::{cache, live::ChatSlot},
    },
};
use eva_core::{GenSink, Provider as _};
use eva_protocol::types::generate::{CacheMode, GenItem};
use futures::{StreamExt as _, channel::mpsc};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub fn schedule<C: Components>(shared: &Shared<C>, slot: &Arc<ChatSlot>) {
    let Some(last) = slot.last_request() else {
        return;
    };
    if last.ttl != cache::Ttl::KeepAlive || last.request.cache == CacheMode::Off {
        return;
    }
    let Some(ctx) = slot.last_ctx() else { return };
    let cancel = CancellationToken::new();
    slot.keep_alive(Some(cancel.clone()));
    let shared = shared.clone();
    let tasks = shared.clone();
    let period = *shared.cfg().context.cache.keep_alive_period;
    let maximum = *shared.cfg().context.cache.keep_alive_max;
    tasks.turns().spawn(async move {
        let expires = tokio::time::Instant::now() + maximum;
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep_until(expires) => break,
                () = tokio::time::sleep(period) => {}
            }
            let entry = shared.models().get(&last.model).cloned();
            let Some(provider) = entry.and_then(|e| shared.provider(&e.provider)) else {
                break;
            };
            let (tx, mut rx) = mpsc::channel(64);
            let run = provider.generate(cache::keep_alive(&last.request), GenSink::new(tx));
            let collect = async {
                let mut usage = Vec::new();
                while let Some(item) = rx.next().await {
                    if let GenItem::Usage(u) = item {
                        usage.push(u);
                    }
                }
                usage
            };
            let result = tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep_until(expires) => break,
                result = async { futures::join!(run, collect) } => result,
            };
            let call = Call {
                model: last.model.clone(),
                origin: ctx.source.clone(),
                op: Op::KeepAlive,
                chat: Some(ctx.chat),
                turn: None,
                run: ctx.run_id.clone(),
                cache_break_reason: None,
            };
            if let Err(e) = Recorder::new(&shared).spend(call, &result.1).await {
                log::warn!(error:% = e; "failed to record keep-alive spend");
            }
            if result.0.is_err() {
                break;
            }
        }
    });
}
