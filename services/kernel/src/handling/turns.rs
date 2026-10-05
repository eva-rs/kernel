//! Туры: авторизация, реплики, подписки и ответы на запросы.

use crate::{
    components::Components,
    db::{Db as _, Queries as _, chats::ChatRow, turns::Delivered as DeliveredState},
    state::{Policy, State},
    subsystems::turns::{
        self, ctx, ingest,
        log::EndedBy,
        pending::{self, Answered},
    },
};
use eva_protocol::{
    errors::{self, Aux, ext::*},
    requests::{
        Response,
        turns::{append_silent, attach, cancel, delivered, respond, start},
    },
    types::{
        ChatId, PersonId,
        turn::{Answer, PendingKind, PendingOutcome, Permission},
    },
};

pub(crate) async fn chat<C: Components, E: From<errors::chats::NotFound>>(
    state: &State<C>,
    chat: ChatId,
) -> Response<ChatRow, E> {
    let row = state
        .shared
        .db()
        .reader()
        .await?
        .run(move |c| c.chats().get(chat))
        .await?
        .ok_or_else(|| errors::chats::NotFound { chat: chat.into() }.spec())?;
    Policy::new(&state.caller).chats().read(&row.chat_ref())?;
    Ok(row)
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Start<C: Components>(
    state: &mut State<C>,
    args: start::Args,
) -> Response<start::Ok, start::Err> {
    let row = chat::<C, start::Err>(state, args.chat).await?;
    validate_content(state, &args.content).await?;
    let actor = ingest::actor(state, row.surface.id, args.actor).await?;
    let mut entry = ingest::entry(&state.shared, &state.caller, args.chat, actor);
    if !args.extra_tools.is_empty() && entry.origin.is_none() {
        return Err(Aux::Invalid("extra_tools require a connected module session".into()).into());
    }
    for tool in &args.extra_tools {
        crate::subsystems::tools::registry::check_tool(tool, state.shared.cfg())
            .map_err(|e| Aux::Invalid(e.into()))?;
    }
    entry.overrides = ctx::Overrides {
        model: args.model,
        effort: args.effort,
        thinking: args.thinking,
    };
    entry.anchor = args.anchor;
    entry.extra_tools = args.extra_tools;
    let result = ingest::append(
        &state.shared,
        entry,
        ingest::Message {
            content: args.content,
            external_id: args.external_id,
            reply_to_external_id: args.reply_to_external_id,
            silent: false,
            machine: false,
        },
    )
    .await?;
    Ok(start::Ok {
        message_id: result.message_id,
        turn: result.turn,
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Attach<C: Components>(
    state: &mut State<C>,
    args: attach::Args,
) -> Response<attach::Ok, attach::Err> {
    chat::<C, attach::Err>(state, args.chat).await?;
    Ok(turns::items::attach(&state.shared, args.chat, args.since_seq).await?)
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn AppendSilent<C: Components>(
    state: &mut State<C>,
    args: append_silent::Args,
) -> Response<append_silent::Ok, append_silent::Err> {
    let row = chat::<C, append_silent::Err>(state, args.chat).await?;
    validate_content(state, &args.content).await?;
    let actor = ingest::actor(state, row.surface.id, args.actor).await?;
    let entry = ingest::entry(&state.shared, &state.caller, args.chat, actor);
    let result = ingest::append(
        &state.shared,
        entry,
        ingest::Message {
            content: args.content,
            external_id: args.external_id,
            reply_to_external_id: None,
            silent: true,
            machine: false,
        },
    )
    .await?;
    Ok(append_silent::Ok {
        message_id: result.message_id,
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Delivered<C: Components>(
    state: &mut State<C>,
    args: delivered::Args,
) -> Response<delivered::Ok, delivered::Err> {
    chat::<C, delivered::Err>(state, args.chat).await?;
    if args.external_id.is_empty() {
        return Err(Aux::Invalid("external_id must not be empty".into()).into());
    }
    let external_id = args.external_id.clone();
    let outcome = state
        .shared
        .db()
        .write(move |tx| {
            tx.turns()
                .set_delivered(args.chat, args.message_id, &external_id)
        })
        .await?;
    match outcome {
        DeliveredState::Ok => Ok(delivered::Ok {}),
        DeliveredState::NotFound => Err(errors::chats::MessageNotFound {
            chat: args.chat,
            message: args.message_id,
        }
        .spec()),
        DeliveredState::AlreadyDelivered { external_id } => Err(errors::chats::AlreadyDelivered {
            message: args.message_id,
            external_id,
        }
        .spec()),
    }
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Cancel<C: Components>(
    state: &mut State<C>,
    args: cancel::Args,
) -> Response<cancel::Ok, cancel::Err> {
    chat::<C, cancel::Err>(state, args.chat).await?;
    let by = state
        .caller
        .module()
        .map_or(EndedBy::Lead, |m| EndedBy::Client { module: m.clone() });
    state.shared.turns().cancel(args.chat, by);
    Ok(cancel::Ok {})
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Respond<C: Components>(
    state: &mut State<C>,
    args: respond::Args,
) -> Response<respond::Ok, respond::Err> {
    let id = args.pending.clone();
    let row = state
        .shared
        .db()
        .reader()
        .await?
        .run(move |c| c.agents().pending(&id))
        .await?
        .ok_or_else(|| {
            errors::turns::PendingNotFound {
                pending: args.pending.clone(),
            }
            .spec()
        })?;
    let chat = row.chat;
    let chat_row = state
        .shared
        .db()
        .reader()
        .await?
        .run(move |c| c.chats().get(chat))
        .await?
        .ok_or_else(|| Aux::NotFound(format!("chat {chat}")))?;
    Policy::new(&state.caller)
        .chats()
        .read(&chat_row.chat_ref())?;
    let actor = ingest::actor(state, chat_row.surface.id, args.actor).await?;
    let turn = row.turn.clone();
    let origin = state
        .shared
        .db()
        .reader()
        .await?
        .run(move |c| c.turns().actor_of_turn(chat, &turn))
        .await?;
    Policy::new(&state.caller).respond(row.kind, &actor, origin.as_ref())?;
    if row.resolved_at.is_some() {
        return Err(errors::turns::AlreadyResolved {
            pending: args.pending,
        }
        .spec());
    }
    if row.kind == PendingKind::Permission && row.expires_at <= state.shared.now() {
        pending::resolve(&state.shared, &row, PendingOutcome::Expired, None).await?;
        return Err(errors::turns::AlreadyResolved {
            pending: args.pending,
        }
        .spec());
    }
    let late = row.kind == PendingKind::Ask && row.expires_at <= state.shared.now();
    let (outcome, answer, late_text) = match (row.kind, args.answer) {
        (PendingKind::Ask, Answer::Text { text }) => (
            PendingOutcome::TextInstead,
            Answered::Text(text.clone()),
            text,
        ),
        (PendingKind::Ask, Answer::Pick { picks }) => {
            let ask: pending::Ask =
                serde_json::from_value(row.payload.clone()).map_err(Aux::internal)?;
            if picks.len() != ask.questions.len() {
                return Err(Aux::Invalid("pick one option for every question".into()).into());
            }
            let mut seen = std::collections::BTreeSet::new();
            let mut labels = Vec::new();
            for p in &picks {
                if !seen.insert(p.question) {
                    return Err(Aux::Invalid("duplicate question".into()).into());
                }
                let choice = ask
                    .questions
                    .get(usize::from(p.question))
                    .and_then(|q| q.options.get(usize::from(p.option)))
                    .ok_or_else(|| Aux::Invalid("pick is outside the offered choices".into()))?;
                labels.push(choice.label.clone());
            }
            (
                PendingOutcome::Answered,
                Answered::Picks(picks),
                labels.join("; "),
            )
        }
        (PendingKind::Permission, Answer::Permission { decision }) => (
            if decision == Permission::Deny {
                PendingOutcome::Denied
            } else {
                PendingOutcome::Allowed
            },
            Answered::Permission(decision),
            String::new(),
        ),
        _ => return Err(Aux::Invalid("answer does not match pending kind".into()).into()),
    };
    let outcome = if late {
        PendingOutcome::LateAnswer
    } else {
        outcome
    };
    let resolved = if let Answered::Permission(decision) = &answer {
        resolve_permission(state, &row, &actor, *decision).await?
    } else {
        pending::resolve(&state.shared, &row, outcome, (!late).then_some(answer)).await?
    };
    if !resolved {
        return Err(errors::turns::AlreadyResolved {
            pending: args.pending,
        }
        .spec());
    }
    if late {
        let entry = ctx::Entry::master(chat, PersonId::system());
        ingest::append(
            &state.shared,
            entry,
            ingest::Message {
                content: vec![eva_protocol::types::content::Block::text(format!(
                    "[ask] late answer to {}: {}",
                    row.call_id, late_text
                ))],
                external_id: None,
                reply_to_external_id: None,
                silent: false,
                machine: true,
            },
        )
        .await?;
    }
    Ok(respond::Ok {})
}

pub(crate) async fn validate_content<C: Components>(
    state: &State<C>,
    blocks: &[eva_protocol::types::content::Block],
) -> Result<(), Aux> {
    use eva_protocol::types::content::Block;
    if blocks.is_empty() {
        return Err(Aux::Invalid("message content must not be empty".into()));
    }
    let refs: Vec<_> = blocks
        .iter()
        .filter_map(|b| match b {
            Block::Image { blob, .. } | Block::File { blob } => Some(blob.clone()),
            Block::Text { .. } => None,
        })
        .collect();
    if refs.is_empty() {
        return Ok(());
    }
    let (caller, now) = (state.caller.clone(), state.shared.now());
    state
        .shared
        .db()
        .reader()
        .await?
        .run(move |c| {
            for id in refs {
                let row = c
                    .blobs()
                    .get(&id)?
                    .filter(|b| b.expires_at > now)
                    .ok_or_else(|| Aux::NotFound(format!("blob {id}")))?;
                match row.chat {
                    Some(chat) => Policy::new(&caller).chats().read(&chat)?,
                    None => Policy::new(&caller).owner()?,
                }
            }
            Ok(())
        })
        .await
}

async fn resolve_permission<C: Components>(
    state: &State<C>,
    row: &crate::db::agents::PendingRow,
    actor: &eva_protocol::types::turn::Actor,
    decision: Permission,
) -> Result<bool, Aux> {
    use crate::db::hands_grants::NewGrant;
    use eva_protocol::types::hands::GrantKind;
    let endpoint = row
        .payload
        .get("endpoint")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Aux::internal("permission has no endpoint"))?
        .to_owned();
    let kind: GrantKind = serde_json::from_value(
        row.payload
            .get("kind")
            .cloned()
            .ok_or_else(|| Aux::internal("permission has no kind"))?,
    )
    .map_err(Aux::internal)?;
    let prefixes: Vec<String> = if kind == GrantKind::Shell {
        vec![
            row.payload
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Aux::internal("permission has no command"))?
                .to_owned(),
        ]
    } else {
        serde_json::from_value(
            row.payload
                .get("paths")
                .cloned()
                .ok_or_else(|| Aux::internal("permission has no paths"))?,
        )
        .map_err(Aux::internal)?
    };
    let session = if decision == Permission::AllowSession {
        let module = state
            .caller
            .module()
            .ok_or_else(|| Aux::Invalid("session permission requires a module".into()))?;
        Some((
            module.clone(),
            state
                .shared
                .live()
                .client(module)
                .ok_or_else(|| Aux::Unavailable("module session ended".into()))?
                .0,
        ))
    } else {
        None
    };
    let now = state.shared.now();
    let outcome = if decision == Permission::Deny {
        PendingOutcome::Denied
    } else {
        PendingOutcome::Allowed
    };
    let grants = if decision == Permission::AllowAlways {
        let chat = row.chat;
        let surface = state
            .shared
            .db()
            .reader()
            .await?
            .run(move |c| c.chats().chat_ref(chat))
            .await?
            .ok_or_else(|| Aux::NotFound(format!("chat {chat}")))?
            .surface
            .id;
        prefixes
            .iter()
            .map(|prefix| NewGrant {
                endpoint: endpoint.clone(),
                surface: surface.clone(),
                chat: Some(chat),
                kind,
                prefix: prefix.clone(),
                issued_by: actor.person_id.clone().unwrap_or_else(PersonId::system),
                created_at: now,
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let id = row.id.clone();
    let expires_at = row.expires_at;
    let shared = state.shared.clone();
    let live = shared.turns().slot(row.chat).live();
    let done = state
        .shared
        .db()
        .write(move |tx| {
            let now = shared.now();
            let outcome = if live.as_ref().is_some_and(|l| l.cancel.is_cancelled()) {
                PendingOutcome::Cancelled
            } else if expires_at <= now {
                PendingOutcome::Expired
            } else {
                outcome
            };
            if !tx.agents().resolve_pending(&id, outcome, now)? {
                return crate::db::DbResult::Ok(None);
            }
            if outcome == PendingOutcome::Allowed {
                for mut grant in grants {
                    grant.created_at = now;
                    tx.hands_grants().insert(&grant)?;
                }
            }
            Ok(Some(outcome))
        })
        .await?;
    let Some(outcome) = done else {
        return Ok(false);
    };
    if matches!(outcome, PendingOutcome::Expired | PendingOutcome::Cancelled) {
        pending::publish_resolution(&state.shared, row, outcome, None).await;
        return Ok(false);
    }
    if let Some((module, session)) = session {
        for prefix in prefixes {
            state.shared.access().allow_session(
                module.clone(),
                session.clone(),
                endpoint.clone(),
                row.chat,
                kind,
                prefix,
            );
        }
    }
    pending::publish_resolution(
        &state.shared,
        row,
        outcome,
        Some(Answered::Permission(decision)),
    )
    .await;
    Ok(true)
}
