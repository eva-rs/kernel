//! История items из журнала и непрерывное переключение на живую ленту.

use super::{
    feed::{self, Origin},
    log::{Event, Payload, ResultContent, SpeakerKind, ThinkingBlob},
};
use crate::{
    components::Components,
    db::{Db as _, Queries as _},
    shared::Shared,
};
use eva_protocol::{
    errors::Aux,
    requests::Turns::attach,
    types::{
        ChatId, EventId,
        chat::Role,
        content::Block,
        tool::ToolContent,
        turn::{
            Decision, FeedEvent, ItemBody, ItemStatus, PendingKind, PlanTask, ToolCallResult,
            TurnItem,
        },
    },
};
use std::collections::BTreeMap;

pub fn text(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Результат меняет item вызова; последующий replay остаётся upsert по call_id.
pub fn history(events: &[Event]) -> Vec<TurnItem> {
    let mut calls = BTreeMap::new();
    let mut out = Vec::new();
    for e in events {
        let mut bodies = Vec::new();
        match &e.payload {
            Payload::Message(m) if m.role == Role::User => bodies.push(ItemBody::UserMessage {
                text: text(&m.blocks),
                sender: m.speaker.as_ref().and_then(|s| s.person_id.clone()),
                master: m
                    .speaker
                    .as_ref()
                    .is_some_and(|s| s.kind == SpeakerKind::Master),
                external_id: m.ext_id.clone(),
            }),
            Payload::Message(m) => {
                bodies.push(ItemBody::AgentMessage {
                    text: text(&m.blocks),
                    cost: None,
                });
            }
            Payload::Thinking(t) => {
                if let ThinkingBlob::Signed {
                    text: Some(text), ..
                } = &t.blob
                {
                    bodies.push(ItemBody::Thinking { text: text.clone() });
                }
            }
            Payload::Reminder(r) => bodies.push(ItemBody::Reminder {
                text: r.text.clone(),
            }),
            Payload::SystemText(s) => bodies.push(ItemBody::Reminder {
                text: s.text.clone(),
            }),
            Payload::ToolUse(t) => {
                calls.insert(t.call_id.clone(), t.clone());
                let item = feed::item(
                    t.call_id.clone(),
                    e.turn_id.as_ref(),
                    e.id,
                    ItemStatus::Running,
                    ItemBody::ToolCall {
                        call_id: t.call_id.clone(),
                        name: t.name.clone(),
                        input: t.input.clone(),
                        agent: None,
                        result: None,
                        progress: None,
                    },
                );
                out.push(replayed(
                    e.id,
                    0,
                    FeedEvent::ItemStarted { item: item.clone() },
                ));
                let mut item = item;
                item.status = ItemStatus::Aborted;
                out.push(replayed(
                    e.id,
                    1,
                    FeedEvent::ItemCompleted {
                        item,
                        replaces: None,
                    },
                ));
            }
            Payload::ToolResult(r) => {
                if let Some(t) = calls.get(&r.call_id) {
                    let mut output: Vec<_> = r
                        .content
                        .iter()
                        .map(|c| match c {
                            ResultContent::Text { text } => ToolContent::text(text.clone()),
                            ResultContent::Image { blob, .. } | ResultContent::File { blob } => {
                                ToolContent::Blob { id: blob.clone() }
                            }
                        })
                        .collect();
                    if let Some(tseq) = r.dedup_of {
                        output.push(ToolContent::text(super::marker::same_as(tseq)));
                    }
                    let status = if r.synthetic.is_some() {
                        ItemStatus::Aborted
                    } else if r.is_error {
                        ItemStatus::Failed
                    } else {
                        ItemStatus::Completed
                    };
                    let item = feed::item(
                        r.call_id.clone(),
                        e.turn_id.as_ref(),
                        e.id,
                        status,
                        ItemBody::ToolCall {
                            call_id: r.call_id.clone(),
                            name: t.name.clone(),
                            input: t.input.clone(),
                            agent: None,
                            result: Some(ToolCallResult {
                                output,
                                is_error: r.is_error,
                            }),
                            progress: None,
                        },
                    );
                    out.push(replayed(
                        e.id,
                        0,
                        FeedEvent::ItemCompleted {
                            item,
                            replaces: None,
                        },
                    ));
                }
            }
            Payload::TurnEnded(t) => out.push(replayed(
                e.id,
                0,
                FeedEvent::TurnDone {
                    turn: t.turn_id.clone(),
                    outcome: t.outcome,
                    error: t.error.clone(),
                },
            )),
            Payload::SegmentReset(r) => bodies.push(ItemBody::Compaction {
                summary_ref: r.new_segment,
            }),
            _ => {}
        }
        if let Payload::Message(m) = &e.payload {
            bodies.extend(attachments(&m.blocks));
        }
        for (n, body) in bodies.into_iter().enumerate() {
            let status = match &e.payload {
                Payload::Message(m) if m.partial => ItemStatus::Aborted,
                _ => ItemStatus::Completed,
            };
            let item = feed::item(
                feed::logged_id(e.id, n),
                e.turn_id.as_ref(),
                e.id,
                status,
                body,
            );
            out.push(replayed(
                e.id,
                n as u32,
                FeedEvent::ItemCompleted {
                    item,
                    replaces: None,
                },
            ));
        }
    }
    out
}

fn replayed(seq: EventId, sub: u32, event: FeedEvent) -> TurnItem {
    TurnItem {
        seq,
        sub,
        replayed: true,
        event,
    }
}

pub async fn attach<C: Components>(
    shared: &Shared<C>,
    chat: ChatId,
    since: Option<EventId>,
) -> Result<attach::Ok, Aux> {
    let slot = shared.turns().slot(chat);
    let mut live = slot.feed.subscribe();
    let snapshot = shared
        .db()
        .reader()
        .await?
        .run(move |c| c.turns().feed_snapshot(chat))
        .await?;
    let high = snapshot.events.last().map_or(EventId(0), |e| e.id);
    slot.feed.note(high);
    let events = super::log::decode_all(snapshot.events).map_err(Aux::from)?;
    let mut replay = if let Some(since) = since {
        history(&events)
            .into_iter()
            .filter(|i| i.seq > since)
            .collect()
    } else {
        Vec::new()
    };
    for (n, p) in snapshot.pending.into_iter().enumerate() {
        let decisions = match p.kind {
            PendingKind::Ask => vec![Decision::Pick, Decision::TextInstead],
            PendingKind::Permission => vec![
                Decision::Allow,
                Decision::AllowSession,
                Decision::AllowAlways,
                Decision::Deny,
            ],
            PendingKind::Unknown => vec![],
        };
        let item = feed::item(
            p.id.to_string(),
            Some(&p.turn),
            high,
            ItemStatus::Running,
            ItemBody::ClientOp {
                pending: p.id,
                kind: p.kind,
                payload: p.payload,
                decisions,
                resolution: p.outcome,
            },
        );
        replay.push(replayed(
            high,
            n as u32,
            FeedEvent::ItemCompleted {
                item,
                replaces: None,
            },
        ));
    }
    if !snapshot.plan.is_empty() {
        let tasks = snapshot
            .plan
            .into_iter()
            .map(|p| PlanTask {
                id: p.task_id,
                phase: p.phase,
                title: p.title,
                status: p.status,
                outcome: p.outcome,
            })
            .collect();
        let item = feed::item(
            feed::plan_id(chat),
            None,
            high,
            ItemStatus::Completed,
            ItemBody::Plan { tasks },
        );
        replay.push(replayed(
            high,
            u32::MAX,
            FeedEvent::ItemCompleted {
                item,
                replaces: None,
            },
        ));
    }
    Ok(attach::Ok(Box::pin(async_stream::stream! {
        for item in replay { yield Ok(item); }
        loop {
            match live.recv().await {
                Ok(msg) => if match msg.origin { Origin::Derived(id)=>id>high, Origin::LiveOnly=>msg.item.seq>=high } { yield Ok(msg.item); },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => { yield Err(Aux::Quota("turn feed subscriber fell behind; reattach from the last sequence".into()).into()); break; },
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })))
}

pub fn attachments(blocks: &[Block]) -> Vec<ItemBody> {
    blocks
        .iter()
        .filter_map(|block| match block {
            Block::Image { blob, media_type } => Some(ItemBody::Image {
                blob: blob.clone(),
                url: format!("eva-blob://{blob}"),
                name: blob.to_string(),
                media_type: media_type.clone(),
            }),
            Block::File { blob } => Some(ItemBody::File {
                blob: blob.clone(),
                url: format!("eva-blob://{blob}"),
                name: blob.to_string(),
                media_type: "application/octet-stream".into(),
            }),
            Block::Text { .. } => None,
        })
        .collect()
}
