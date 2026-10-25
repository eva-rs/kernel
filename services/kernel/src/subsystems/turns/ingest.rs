//! Реплика и запуск тура под одними воротами чата.

use super::{
    compaction,
    ctx::{self, Entry},
    engine::Turn,
    items,
    log::{self, Append, MessagePayload, Payload, Speaker, SpeakerKind},
    pending::{self, Answered},
};
use crate::{
    components::Components,
    db::{Db as _, Queries as _, turns::NewMessage},
    markers::{sanitize_inline, sanitize_inline_bounded},
    shared::Shared,
    state::State,
};
use eva_core::Caller;
use eva_protocol::{
    errors::{Aux, Generic},
    requests::Persons::resolve,
    types::{
        ChatId, EventId, MessageId, PersonId, SurfaceId,
        content::Block,
        turn::{
            Actor, ActorKind, ActorRef, Audience, FeedEvent, PendingKind, PendingOutcome, TurnState,
        },
    },
};

pub struct Message {
    pub content: Vec<Block>,
    pub external_id: Option<String>,
    pub reply_to_external_id: Option<String>,
    pub silent: bool,
    pub machine: bool,
}

pub struct Appended {
    pub message_id: MessageId,
    pub turn: TurnState,
}

pub async fn actor<C: Components>(
    state: &mut State<C>,
    surface: SurfaceId,
    actor: Option<ActorRef>,
) -> Result<Actor, Aux> {
    let Some(actor) = actor else {
        return Ok(Actor::unknown());
    };
    let handle = actor.handle.clone();
    let person = crate::handling::persons::resolve_person(
        state,
        resolve::Args {
            surface,
            external_id: actor.external_id,
            handle: actor.handle,
            name: actor.name,
        },
    )
    .await
    .map_err(|e| match e {
        Generic::Aux(e) => e,
        Generic::Spec(e) => match e {},
    })?
    .person;
    Ok(Actor {
        person_id: Some(person.id),
        kind: if person.master {
            ActorKind::Master
        } else if person.whitelisted {
            ActorKind::Whitelisted
        } else {
            ActorKind::Unknown
        },
        name: Some(sanitize_inline(&person.name)),
        handle: handle.map(|h| sanitize_inline(&h)),
    })
}

pub fn entry<C: Components>(
    shared: &Shared<C>,
    caller: &Caller,
    chat: ChatId,
    actor: Actor,
) -> Entry {
    Entry {
        chat,
        actor,
        overrides: Default::default(),
        whitelist: Default::default(),
        run_id: None,
        sleep_run_id: None,
        anchor: None,
        extra_tools: Vec::new(),
        origin: caller
            .module()
            .and_then(|m| shared.live().client(m).map(|(s, _)| (m.clone(), s))),
        source: crate::subsystems::stats::Origin::Surface,
    }
}

pub async fn append<C: Components>(
    shared: &Shared<C>,
    entry: Entry,
    message: Message,
) -> Result<Appended, Aux> {
    let chat = entry.chat;
    if message.content.is_empty() {
        return Err(Aux::Invalid("message content must not be empty".into()));
    }
    if message.external_id.as_ref().is_some_and(|s| s.is_empty()) {
        return Err(Aux::Invalid("external_id must not be empty".into()));
    }
    let slot = shared.turns().slot(chat);
    let _gate = slot.gate.lock().await;
    if slot.ingest_paused() {
        return Err(Aux::Unavailable("chat is being deleted".into()));
    }
    slot.keep_alive(None);
    if let Some(ext) = message.external_id.clone()
        && let Some(known) = shared
            .db()
            .reader()
            .await?
            .run(move |c| c.turns().by_external_id(chat, &ext))
            .await?
    {
        return Ok(Appended {
            message_id: known.event,
            turn: known.turn.map_or(TurnState::None, TurnState::Live),
        });
    }
    let llm = shared.cfg().llm.clone();
    let mut ctx = shared
        .db()
        .reader()
        .await?
        .run(move |c| ctx::resolve(c, entry, &llm))
        .await?;
    let mixed = ctx.actor.kind != ActorKind::Master && !message.machine;
    if mixed && ctx.audience == Audience::MasterOnly {
        ctx.audience = Audience::Mixed;
        shared
            .db()
            .write(move |tx| tx.chats().mark_reset(chat))
            .await?;
    }
    let existing = slot.live();
    if existing.is_none() {
        if super::gates::idle_reset(shared, &ctx).await? {
            compaction::reset(shared, &ctx).await?;
        }
        let reset = shared
            .db()
            .reader()
            .await?
            .run(move |c| c.turns().reset_pending(chat))
            .await?;
        if reset {
            compaction::reset(shared, &ctx).await?;
        }
    }
    let segment = compaction::ensure_segment(shared, &ctx).await?;
    let start = !message.silent && ctx.actor.kind != ActorKind::Unknown;
    let turn = if !start {
        None
    } else {
        existing
            .as_ref()
            .map(|l| l.turn.clone())
            .or_else(|| start.then(|| shared.make_world().mint()))
    };
    let now = shared.now();
    let actor = ctx.actor.clone();
    let title_text = (!message.machine
        && actor
            .person_id
            .as_ref()
            .is_none_or(|id| id.as_str() != "p0"))
    .then(|| items::text(&message.content));
    let pending = shared
        .db()
        .reader()
        .await?
        .run(move |c| c.agents().open_pending(chat))
        .await?;
    let allowed = actor.kind == ActorKind::Master
        || slot
            .last_ctx()
            .is_some_and(|c| actor.person_id.is_some() && actor.person_id == c.actor.person_id);
    let answer = if allowed && start {
        pending
            .iter()
            .find(|p| {
                p.kind == PendingKind::Ask && p.expires_at > now && turn.as_ref() == Some(&p.turn)
            })
            .cloned()
    } else {
        None
    };
    let mut payload = if message.machine {
        MessagePayload::kernel(message.content)
    } else {
        MessagePayload::user(
            Speaker {
                person_id: actor.person_id.clone(),
                kind: match actor.kind {
                    ActorKind::Master => SpeakerKind::Master,
                    ActorKind::Whitelisted => SpeakerKind::User,
                    ActorKind::Unknown => SpeakerKind::Unknown,
                },
                name: actor.name.map(|name| {
                    sanitize_inline_bounded(
                        &name,
                        shared.cfg().chats.marker.name_tokens,
                        shared.cfg().context.ratio_default,
                    )
                }),
                handle: actor.handle.map(|handle| {
                    sanitize_inline_bounded(
                        &handle,
                        shared.cfg().chats.marker.name_tokens,
                        shared.cfg().context.ratio_default,
                    )
                }),
            },
            message.content,
        )
    };
    payload.ext_id = message.external_id;
    let pending_answer = answer.clone();
    let (saved, turn_id, reply, who) = (
        payload.clone(),
        turn.clone(),
        message.reply_to_external_id,
        if message.machine {
            Some(PersonId::from("p0"))
        } else {
            ctx.actor.person_id.clone()
        },
    );
    let zone = shared.day_zone();
    let snippet_bytes = (f64::from(shared.cfg().chats.marker.snippet_tokens)
        * shared.cfg().context.ratio_default) as usize;
    let (event, answered, restored) = shared
        .db()
        .write(move |tx| {
            let mut saved = saved;
            let answered = if let Some(p) = pending_answer {
                let won = tx
                    .agents()
                    .resolve_pending(&p.id, PendingOutcome::TextInstead, now)?;
                if won {
                    saved.consumed_by = Some(p.call_id);
                }
                won
            } else {
                false
            };
            if let Some(reply) = &reply
                && let Some(known) = tx.turns().by_external_id(chat, reply)?
            {
                let events = log::decode_all(
                    tx.turns()
                        .events_after(chat, Some(EventId(known.event.0 - 1)))?,
                )
                .map_err(Aux::from)?;
                if let Some(target) = events.iter().find(|e| e.id == known.event)
                    && let Payload::Message(m) = &target.payload
                {
                    saved.reply = Some(log::Reply {
                        ext_id: reply.clone(),
                        yours: m.role == eva_protocol::types::chat::Role::Assistant,
                        snippet: (target.segment != segment).then(|| {
                            let mut text = sanitize_inline(&items::text(&m.blocks));
                            let mut end = text.len().min(snippet_bytes);
                            while !text.is_char_boundary(end) {
                                end -= 1;
                            }
                            text.truncate(end);
                            text
                        }),
                    });
                }
            }
            let id = log::append(
                tx,
                Append {
                    chat,
                    segment,
                    turn_id: turn_id.as_ref(),
                    step: None,
                    now,
                },
                &Payload::Message(saved.clone()),
            )?;
            tx.turns().insert_message(&NewMessage {
                chat,
                event: id,
                role: saved.role,
                sender: who.as_ref(),
                external_id: saved.ext_id.as_deref(),
                reply_to_external_id: reply.as_deref(),
                turn_id: turn_id.as_ref(),
            })?;
            tx.turns().touch_chat(chat, now)?;
            let restored = if tx
                .chats()
                .get(chat)?
                .is_some_and(|c| c.deleted_at.is_some())
            {
                tx.chats().set_deleted(chat, None)?;
                tx.stats()
                    .bump(crate::db::stats::Metric::ChatsAutoRestored, "", now, &zone)?;
                tx.chats().get(chat)?
            } else {
                None
            };
            Result::<_, Aux>::Ok((id, answered, restored))
        })
        .await?;
    if let Some(text) = title_text {
        crate::subsystems::titles::schedule(shared, chat, event, text);
    }
    if let Some(row) = restored {
        shared.bus().publish(
            eva_protocol::wire::events::Event::ChatRestored(
                eva_protocol::wire::events::ChatEvent {
                    chat,
                    surface: row.surface.id.clone(),
                    external_id: row.alias.map(|a| a.external_id),
                },
            ),
            Some(row.surface),
        );
    }
    slot.feed.note(event);
    slot.feed.derived(
        event,
        0,
        FeedEvent::ItemCompleted {
            item: super::feed::item(
                super::feed::logged_id(event, 0),
                turn.as_ref(),
                event,
                eva_protocol::types::turn::ItemStatus::Completed,
                eva_protocol::types::turn::ItemBody::UserMessage {
                    text: items::text(&payload.blocks),
                    sender: ctx.actor.person_id.clone(),
                    master: ctx.master_turn,
                    external_id: payload.ext_id,
                },
            ),
            replaces: None,
        },
    );
    for (n, body) in items::attachments(&payload.blocks).into_iter().enumerate() {
        slot.feed.derived(
            event,
            (n + 1) as u32,
            FeedEvent::ItemCompleted {
                item: super::feed::item(
                    super::feed::logged_id(event, n + 1),
                    turn.as_ref(),
                    event,
                    eva_protocol::types::turn::ItemStatus::Completed,
                    body,
                ),
                replaces: None,
            },
        );
    }
    for p in pending
        .iter()
        .filter(|p| start && p.kind == PendingKind::Ask && p.expires_at <= now)
    {
        pending::resolve(shared, p, PendingOutcome::Superseded, None).await?;
    }
    if let Some(p) = answer.filter(|_| answered) {
        pending::publish_resolution(
            shared,
            &p,
            PendingOutcome::TextInstead,
            Some(Answered::Text(items::text(&payload.blocks))),
        )
        .await;
    }
    let state = if !start {
        TurnState::None
    } else if let Some(live) = existing {
        live.wake.notify_one();
        TurnState::Live(live.turn)
    } else if let Some(turn) = turn {
        let live = shared.turns().claim(&slot, turn.clone());
        slot.set_ctx(ctx.clone());
        let task = Turn::new(shared.clone(), slot.clone(), live, ctx, segment);
        shared.turns().spawn(task.run());
        TurnState::Started(turn)
    } else {
        TurnState::None
    };
    Ok(Appended {
        message_id: event,
        turn: state,
    })
}
