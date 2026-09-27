//! Рассылка шины модулю (`06 §7`): `notify events.publish` по подписке
//! capability `events`, фильтр — `Policy::events().deliver`. `seq` — на
//! сессию, реплея после реконнекта нет.

use std::collections::BTreeSet;

use tokio::sync::broadcast::{Receiver, error::RecvError};

use eva_core::Caller;
use eva_protocol::wire::events::{Publish, Topic};
use eva_rpc::ModuleClient;

use crate::{bus::Published, components::Components, shared::Shared, state::Policy};

/// `bus` взят на handshake, до `welcome`; рассылка живёт, пока живо соединение.
pub fn forward<C: Components>(
    shared: Shared<C>,
    caller: Caller,
    topics: BTreeSet<Topic>,
    module: ModuleClient,
    mut bus: Receiver<Published>,
) {
    tokio::spawn(async move {
        let mut seq = 0;
        loop {
            let published = tokio::select! {
                _ = module.peer().closed() => break,
                published = bus.recv() => published,
            };
            let published = match published {
                Ok(published) => published,
                Err(RecvError::Lagged(missed)) => {
                    log::warn!(module = caller.label(), missed; "module fell behind the event bus");
                    continue;
                }
                Err(RecvError::Closed) => break,
            };

            let topic = published.event.topic();
            if !topics.contains(&topic)
                || Policy::new(&caller)
                    .events()
                    .deliver(&topic, published.surface.as_ref())
                    .is_err()
            {
                continue;
            }

            let publish = Publish {
                seq,
                at: shared.now(),
                event: published.event,
            };
            seq += 1;
            if let Err(e) = module.publish(&publish).await {
                log::debug!(module = caller.label(), error:% = e; "event delivery stopped");
                break;
            }
        }
    });
}
