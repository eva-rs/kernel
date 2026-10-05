//! Pending-запросы (`07 §4`): `chat_ask` блокирует вызов до ответа или
//! `work.ask_ttl`; просроченный `ask` остаётся открытым до кнопки
//! (`late_answer`) или следующей реплики (`superseded`). Любой исход —
//! `request_resolved` в ленте.

use std::{sync::Arc, time::Duration};

use eva::data;
use serde_json::{Value, json};

use eva_protocol::{
    errors::Aux,
    types::{
        ChatId, PendingId, TurnId,
        tool::ToolContent,
        turn::{Decision, FeedEvent, ItemBody, ItemStatus, PendingKind, PendingOutcome, Pick},
    },
    wire::tools::ToolResult,
};

use crate::{
    components::Components,
    db::{Db as _, Queries as _, agents::PendingRow, stats::Metric},
    shared::Shared,
    subsystems::turns::{engine::Turn, feed, live::ChatSlot, marker},
};

/// Ask the human: 1-3 questions, each with 2-8 options.
#[data]
pub struct Ask {
    pub questions: Vec<Question>,
}

#[data]
pub struct Question {
    pub text: String,
    pub options: Vec<Choice>,
}

#[data]
pub struct Choice {
    pub label: String,
}

/// Ответ, которого ждёт вызов.
#[derive(Debug, Clone, PartialEq)]
pub enum Answered {
    Picks(Vec<Pick>),
    /// Реплика человека вместо кнопок: уже в логе с `consumed_by`.
    Text(String),
    Permission(eva_protocol::types::turn::Permission),
}

fn check(ask: &Ask) -> Result<(), String> {
    if !(1..=3).contains(&ask.questions.len()) {
        return Err(String::from("invalid input: 1..=3 questions"));
    }
    for q in &ask.questions {
        if q.text.trim().is_empty() || !(2..=8).contains(&q.options.len()) {
            return Err(String::from(
                "invalid input: every question needs text and 2..=8 options",
            ));
        }
        if q.options.iter().any(|o| o.label.trim().is_empty()) {
            return Err(String::from(
                "invalid input: option labels must not be empty",
            ));
        }
    }
    Ok(())
}

/// `chat_ask` (`07 §4.1`).
pub async fn ask<C: Components>(
    turn: &Turn<C>,
    call_id: &str,
    args: Ask,
) -> Result<ToolResult, Aux> {
    if let Err(e) = check(&args) {
        return Ok(ToolResult::error(e));
    }
    let shared = &turn.shared;
    let ttl = *shared.cfg().work.ask_ttl;
    let now = shared.now();
    let payload = serde_json::to_value(&args).map_err(|e| Aux::Internal(e.to_string()))?;
    let row = PendingRow {
        id: shared.make_world().mint(),
        chat: turn.ctx.chat,
        turn: turn.live.turn.clone(),
        call_id: call_id.to_owned(),
        kind: PendingKind::Ask,
        payload: payload.clone(),
        created_at: now,
        expires_at: eva::time::Timestamp::from_nanos(
            now.as_nanos()
                .saturating_add(u64::try_from(ttl.as_nanos()).unwrap_or(u64::MAX)),
        ),
        resolved_at: None,
        outcome: None,
    };
    let answer = shared.turns().wait_answer(row.id.clone());
    let insert = row.clone();
    shared
        .db()
        .write(move |tx| tx.agents().insert_pending(&insert))
        .await?;
    turn.slot.feed.live(FeedEvent::ItemStarted {
        item: feed::item(
            row.id.to_string(),
            Some(&turn.live.turn),
            turn.slot.feed.last(),
            ItemStatus::Running,
            ItemBody::ClientOp {
                pending: row.id.clone(),
                kind: PendingKind::Ask,
                payload,
                decisions: vec![Decision::Pick, Decision::TextInstead],
                resolution: None,
            },
        ),
    });

    let answered = tokio::select! {
        answer = answer => answer.ok(),
        () = tokio::time::sleep(ttl) => None,
        () = turn.live.cancel.cancelled() => {
            shared.turns().forget_waiter(&row.id);
            return Ok(ToolResult::error("cancelled"));
        }
    };
    let Some(answered) = answered else {
        shared.turns().forget_waiter(&row.id);
        shared.bump(Metric::PendingExpired, "ask").await;
        return Ok(ToolResult::ok(vec![ToolContent::text(marker::no_answer(
            ttl,
        ))]));
    };
    Ok(match answered {
        Answered::Picks(picks) => {
            let answers: Vec<Value> = picks
                .iter()
                .filter_map(|p| {
                    let q = args.questions.get(usize::from(p.question))?;
                    let o = q.options.get(usize::from(p.option))?;
                    Some(json!({"question": p.question, "picked": p.option, "label": o.label}))
                })
                .collect();
            ToolResult::ok(vec![ToolContent::text(
                json!({"answers": answers}).to_string(),
            )])
        }
        Answered::Text(text) => ToolResult::ok(vec![ToolContent::text(marker::quote(&text))]),
        Answered::Permission(_) => ToolResult::error("unexpected permission answer"),
    })
}

/// Разрешение pending: исход в БД, `request_resolved` в ленту, ждущий вызов
/// просыпается. `false` — уже разрешён.
pub async fn resolve<C: Components>(
    shared: &Shared<C>,
    row: &PendingRow,
    outcome: PendingOutcome,
    answer: Option<Answered>,
) -> Result<bool, Aux> {
    let (id, now) = (row.id.clone(), shared.now());
    let done = shared
        .db()
        .write(move |tx| tx.agents().resolve_pending(&id, outcome, now))
        .await?;
    if !done {
        return Ok(false);
    }
    publish_resolution(shared, row, outcome, answer).await;
    Ok(true)
}

/// Публикует уже зафиксированное разрешение и будит ожидающий вызов.
pub async fn publish_resolution<C: Components>(
    shared: &Shared<C>,
    row: &PendingRow,
    outcome: PendingOutcome,
    answer: Option<Answered>,
) {
    if let Some(answer) = answer {
        shared.turns().answer(&row.id, answer);
    } else {
        shared.turns().forget_waiter(&row.id);
    }
    let slot = shared.turns().slot(row.chat);
    resolved(&slot, &row.id, outcome);
    let kind = kind_str(row.kind);
    let metric = match outcome {
        PendingOutcome::Cancelled => Metric::PendingCancelled,
        PendingOutcome::Expired => Metric::PendingExpired,
        _ => Metric::PendingResolved,
    };
    shared.bump(metric, kind).await;
}

fn resolved(slot: &ChatSlot, pending: &PendingId, outcome: PendingOutcome) {
    slot.feed.live(FeedEvent::RequestResolved {
        pending: pending.clone(),
        outcome,
    });
}

const fn kind_str(kind: PendingKind) -> &'static str {
    match kind {
        PendingKind::Ask => "ask",
        PendingKind::Permission => "permission",
        PendingKind::Unknown => "unknown",
    }
}

/// Тур кончился отменой или ошибкой: его pending → `cancelled` (`07 §8` п.4).
pub async fn cancel_turn<C: Components>(shared: &Shared<C>, slot: &Arc<ChatSlot>, turn: &TurnId) {
    let (turn_id, now) = (turn.clone(), shared.now());
    let cancelled = shared
        .db()
        .write(move |tx| tx.agents().cancel_pending(Some(&turn_id), now))
        .await;
    match cancelled {
        Ok(rows) => {
            for row in rows {
                shared.turns().forget_waiter(&row.id);
                resolved(slot, &row.id, PendingOutcome::Cancelled);
                shared
                    .bump(Metric::PendingCancelled, kind_str(row.kind))
                    .await;
            }
        }
        Err(e) => log::error!(turn:% = turn, error:% = e; "failed to cancel pending of the turn"),
    }
}

/// Открытый `ask` тура чата, которому реплика актора тура или хозяина
/// отвечает текстом (`05 §3` п.7).
pub async fn open_ask<C: Components>(
    shared: &Shared<C>,
    chat: ChatId,
    turn: &TurnId,
) -> Result<Option<PendingRow>, Aux> {
    let turn = turn.clone();
    Ok(shared
        .db()
        .reader()
        .await?
        .run(move |c| c.agents().open_pending(chat))
        .await?
        .into_iter()
        .find(|p| p.kind == PendingKind::Ask && p.turn == turn))
}

/// Длительность `ask_ttl` словами для `no answer within …`.
pub fn ttl_words(ttl: Duration) -> String {
    let secs = ttl.as_secs();
    if secs >= 60 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionOutcome {
    Allowed,
    Denied,
    Expired,
    Cancelled,
    Unavailable,
}

/// Ожидание разрешения рук; таймаут самой операции начинается после возврата.
pub async fn permission<C: Components>(
    shared: &Shared<C>,
    ctx: &eva_protocol::types::turn::TurnCtx,
    call_id: &str,
    payload: Value,
) -> Result<PermissionOutcome, Aux> {
    let chat = ctx.chat;
    let surface = shared
        .db()
        .reader()
        .await?
        .run(move |c| c.chats().get(chat))
        .await?
        .ok_or_else(|| Aux::NotFound(format!("chat {chat} does not exist")))?
        .surface;
    let has_client = shared.live().has_client();
    if !has_client && !shared.live().ask_ui(&surface) {
        return Ok(PermissionOutcome::Unavailable);
    }
    let ttl = shared.cfg().work.permission_ttl.0;
    let now = shared.now();
    let row = PendingRow {
        id: shared.make_world().mint(),
        chat,
        turn: ctx.turn_id.clone(),
        call_id: call_id.to_owned(),
        kind: PendingKind::Permission,
        payload: payload.clone(),
        created_at: now,
        expires_at: crate::db::stats::later(now, ttl),
        resolved_at: None,
        outcome: None,
    };
    let answer = shared.turns().wait_answer(row.id.clone());
    let insert = row.clone();
    shared
        .db()
        .write(move |tx| tx.agents().insert_pending(&insert))
        .await?;
    let slot = shared.turns().slot(chat);
    slot.feed.live(FeedEvent::ItemStarted {
        item: feed::item(
            row.id.to_string(),
            Some(&ctx.turn_id),
            slot.feed.last(),
            ItemStatus::Running,
            ItemBody::ClientOp {
                pending: row.id.clone(),
                kind: PendingKind::Permission,
                payload,
                decisions: vec![
                    Decision::Allow,
                    Decision::AllowSession,
                    Decision::AllowAlways,
                    Decision::Deny,
                ],
                resolution: None,
            },
        ),
    });
    let cancel = slot.live().map(|live| live.cancel).unwrap_or_default();
    let outcome = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            resolve(shared, &row, PendingOutcome::Cancelled, None).await?;
            PermissionOutcome::Cancelled
        },
        answer = answer => match answer.ok() {
            Some(Answered::Permission(eva_protocol::types::turn::Permission::Allow | eva_protocol::types::turn::Permission::AllowSession | eva_protocol::types::turn::Permission::AllowAlways)) => PermissionOutcome::Allowed,
            Some(_) => PermissionOutcome::Denied,
            None => PermissionOutcome::Cancelled,
        },
        () = tokio::time::sleep(ttl) => {
            resolve(shared, &row, PendingOutcome::Expired, None).await?;
            PermissionOutcome::Expired
        },
    };
    Ok(outcome)
}
