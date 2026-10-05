//! Хуки модулей (`06 §7`): цепочка по именам модулей, скоуп как у событий,
//! таймаут под потолком `modules.hooks.timeout`, отказ — по `fail_mode`.
//! Патч есть только у `after_tool` и идёт следующему хуку цепочки.

use serde::{Serialize, de::DeserializeOwned};

use eva_protocol::{
    types::ModuleId,
    wire::{
        ErrorCode,
        capabilities::FailMode,
        hooks::{AfterTool, BeforeTool, BeforeTurn, Decision, Hook, NoPatch},
    },
};

use crate::{
    components::Components, db::stats::Metric, shared::Shared, state::Policy,
    subsystems::turns::ctx::Ctx,
};

/// Вход точки, к которому применяется патч предыдущего хука.
pub trait Point: Hook + Serialize + Clone + Send + Sync
where
    Self::Patch: DeserializeOwned + Send,
{
    fn apply(&mut self, patch: Self::Patch);
}

impl Point for BeforeTurn {
    fn apply(&mut self, patch: NoPatch) {
        match patch {}
    }
}

impl Point for BeforeTool {
    fn apply(&mut self, patch: NoPatch) {
        match patch {}
    }
}

impl Point for AfterTool {
    fn apply(&mut self, patch: Self::Patch) {
        self.result = patch;
    }
}

/// Итог цепочки.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict<H> {
    /// Вход после патчей.
    Continue(H),
    Cancel {
        module: ModuleId,
        reason: String,
    },
}

pub async fn chain<C, H>(shared: &Shared<C>, ctx: &Ctx, input: &H) -> Verdict<H>
where
    C: Components,
    H: Point,
    H::Patch: DeserializeOwned + Send,
{
    let ceiling = *shared.cfg().modules.hooks.timeout;
    let mut current = input.clone();
    for target in shared.live().hooked(H::POINT) {
        if Policy::new(&target.caller)
            .events()
            .hook(&ctx.surface)
            .is_err()
        {
            continue;
        }
        let limit = target.cap.timeout.map_or(ceiling, |t| t.0.min(ceiling));
        let answer = match &target.client {
            Some(client) => client.hook(&current, limit).await,
            None => Err(eva_protocol::wire::RpcError::new(
                ErrorCode::Unavailable,
                "module session is not ready",
            )),
        };
        let failure = match answer {
            Ok(Decision::Continue) => continue,
            Ok(Decision::Patch { payload }) => {
                current.apply(payload);
                continue;
            }
            Ok(Decision::Cancel { reason }) => {
                return Verdict::Cancel {
                    module: target.module,
                    reason,
                };
            }
            Ok(Decision::Unknown) => Metric::HooksError,
            Err(e) if e.code == ErrorCode::Timeout => Metric::HooksTimeout,
            Err(_) => Metric::HooksError,
        };
        log::warn!(module:% = target.module, point:% = H::POINT, fail_mode:% = target.cap.fail_mode; "hook failed");
        shared.bump(failure, target.module.as_str()).await;
        if target.cap.fail_mode == FailMode::Closed {
            return Verdict::Cancel {
                reason: format!("hook {}.{} unavailable", target.module, H::POINT),
                module: target.module,
            };
        }
    }
    Verdict::Continue(current)
}
