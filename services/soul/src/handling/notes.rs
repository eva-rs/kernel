//! Блокнот хозяина: мутации и журнал снимков атомарны; режим меняет вход в тур.

use std::collections::BTreeSet;

use eva_protocol::{
    errors::{self, Aux, ext::*},
    requests::{
        Notes::{delete, history, list, put, restore, snapshot},
        Response,
    },
    types::{
        ChatId,
        note::{Mode, NoteUpdated, tools as wire},
    },
};

use crate::{
    components::Components,
    convert,
    db::{Db as _, Queries as _},
    state::{Policy, State},
    subsystems::{
        notes::{budget, state, tools},
        turns::{
            estimate::Estimator,
            log::{self, Append, Payload},
        },
    },
};

#[eva::endpoint]
#[fastrace::trace]
pub async fn List<C: Components>(
    state: &mut State<C>,
    args: list::Args,
) -> Response<list::Ok, list::Err> {
    let State {
        shared,
        caller,
        db_conn,
        ..
    } = state;
    Policy::new(caller).notes().read()?;
    let list::Args { chat } = args;
    let cfg = &shared.cfg().notes;
    cfg.enabled.true_or(Aux::Unavailable(
        "notes are disabled by notes.enabled".into(),
    ))?;

    let default_model = shared.cfg().llm.default_model.clone();
    let db = db_conn.acquire(shared).await?;
    let (model, notes, notes_state, ratio) = db
        .run(move |c| -> Response<_, list::Err> {
            let model = c
                .notes()
                .chat_model(chat)?
                .ok_or_else(|| errors::chats::NotFound { chat: chat.into() }.spec())?
                .unwrap_or(default_model);
            let notes = c.notes().list(chat)?;
            let state = c.notes().state(chat)?;
            let ratio = c.turns().ratio(&model)?;
            Ok((model, notes, state, ratio))
        })
        .await?;

    let window = shared.models().get(&model).map(|m| m.decl.context_window);
    let budget = budget::budget(window, cfg);
    let ratio = Some(Estimator::new(ratio, &shared.cfg().context).ratio);
    let fill = budget::fill(&notes, budget, ratio);
    let notes_state = notes_state.unwrap_or_else(|| state::initial(shared.now()));

    Ok(list::Ok {
        notes: notes.into_iter().map(convert::note::from_db).collect(),
        mode: notes_state.mode,
        squeeze_turns: notes_state.squeeze_turns,
        tokens: fill.tokens,
        budget,
    })
}

fn enabled<C: Components>(state: &State<C>) -> Result<(), Aux> {
    state.shared.cfg().notes.enabled.true_or(Aux::Unavailable(
        "notes are disabled by notes.enabled".into(),
    ))
}

fn valid_snapshot(id: i64) -> Result<(), Aux> {
    (id > 0).true_or(Aux::Invalid("snapshot id must be positive".into()))
}

fn tool_error(error: tools::Error) -> Aux {
    match error {
        tools::Error::Db(error) => error.into(),
        tools::Error::Rejected(error) if error.reason == "not_found" => {
            Aux::NotFound(error.message.trim_end_matches(" not found").into())
        }
        tools::Error::Rejected(error) => Aux::Invalid(error.message),
    }
}

/// Gate не даёт начать тур между проверкой idle и commit внешней правки.
async fn edit<C, T, F>(state: &mut State<C>, chat: ChatId, change: F) -> Response<T, list::Err>
where
    C: Components,
    T: Send + 'static,
    F: FnOnce(
            &crate::db::Connection,
            &crate::config::NotesConfig,
            &mut tools::Ctx<'_>,
        ) -> Result<(T, Vec<NoteUpdated>), tools::Error>
        + Send
        + 'static,
{
    enabled(state)?;
    let shared = state.shared.clone();
    let slot = shared.turns().slot(chat);
    let gate = slot.gate.clone().lock_owned().await;
    if slot.live().is_some() {
        return Err(Aux::Conflict(
            "finish or cancel the live turn before editing its notebook".into(),
        )
        .into());
    }
    let soul = shared.clone();
    let (result, _gate) = shared
        .db()
        .write(move |tx| -> Response<_, list::Err> {
            let row = tx
                .chats()
                .get(chat)?
                .ok_or_else(|| errors::chats::NotFound { chat: chat.into() }.spec())?;
            if row.deleted_at.is_some() {
                return Err(Aux::Conflict(
                    "restore the deleted chat before editing its notebook".into(),
                )
                .into());
            }
            let cfg = &soul.cfg().notes;
            let model = tx
                .notes()
                .chat_model(chat)?
                .flatten()
                .unwrap_or_else(|| soul.cfg().llm.default_model.clone());
            let window = soul
                .models()
                .get(&model)
                .map(|model| model.decl.context_window);
            let budget = budget::budget(window, cfg);
            let ratio = Some(Estimator::new(tx.turns().ratio(&model)?, &soul.cfg().context).ratio);
            let mode = tx
                .notes()
                .state(chat)?
                .map_or(Mode::Inline, |state| state.mode);
            // Ручная правка — явное решение хозяина; модельный read-before-write остаётся в tools.
            let mut read: BTreeSet<_> = tx
                .notes()
                .list(chat)?
                .into_iter()
                .map(|note| note.name)
                .collect();
            let now = soul.now();
            let mut ctx = tools::Ctx {
                chat,
                mode,
                read_this_turn: &mut read,
                ratio,
                budget,
                now,
            };
            let (result, events) = change(tx, cfg, &mut ctx).map_err(tool_error)?;
            if let Some(segment) = tx.turns().open_segment(chat)? {
                for event in events {
                    log::append(
                        tx,
                        Append {
                            chat,
                            segment: segment.id,
                            turn_id: None,
                            step: None,
                            now,
                        },
                        &Payload::NoteUpdated(event),
                    )?;
                }
            }
            // Guard остаётся в результате писателя до завершения commit даже при отмене RPC.
            Ok((result, gate))
        })
        .await?;
    Ok(result)
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Put<C: Components>(
    state: &mut State<C>,
    args: put::Args,
) -> Response<put::Ok, put::Err> {
    Policy::new(&state.caller).notes().write()?;
    edit(state, args.chat, move |tx, cfg, ctx| {
        let applied = tools::save(
            tx,
            cfg,
            ctx,
            wire::save::Args {
                name: args.name,
                description: args.description,
                body: args.body,
                pinned: args.pinned,
            },
        )?;
        Ok((applied.ok.into(), vec![applied.event]))
    })
    .await
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Delete<C: Components>(
    state: &mut State<C>,
    args: delete::Args,
) -> Response<delete::Ok, delete::Err> {
    Policy::new(&state.caller).notes().write()?;
    edit(state, args.chat, move |tx, cfg, ctx| {
        let applied = tools::delete(tx, cfg, ctx, wire::delete::Args { name: args.name })?;
        Ok((applied.ok.into(), vec![applied.event]))
    })
    .await
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn History<C: Components>(
    state: &mut State<C>,
    args: history::Args,
) -> Response<history::Ok, history::Err> {
    Policy::new(&state.caller).notes().read()?;
    enabled(state)?;
    if let Some(id) = args.before {
        valid_snapshot(id)?;
    }
    let db = state.db_conn.acquire(&state.shared).await?;
    let rows = db
        .run(move |tx| -> Response<_, history::Err> {
            tx.chats().get(args.chat)?.ok_or_else(|| {
                errors::chats::NotFound {
                    chat: args.chat.into(),
                }
                .spec()
            })?;
            Ok(tx
                .notes()
                .history(args.chat, args.before, args.limit.into_inner().into())?)
        })
        .await?;
    Ok(history::Ok {
        snapshots: rows.into_iter().map(convert::note::snapshot).collect(),
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Snapshot<C: Components>(
    state: &mut State<C>,
    args: snapshot::Args,
) -> Response<snapshot::Ok, snapshot::Err> {
    Policy::new(&state.caller).notes().read()?;
    enabled(state)?;
    valid_snapshot(args.id)?;
    let db = state.db_conn.acquire(&state.shared).await?;
    let (snapshot, notes) = db
        .run(move |tx| -> Response<_, snapshot::Err> {
            tx.chats().get(args.chat)?.ok_or_else(|| {
                errors::chats::NotFound {
                    chat: args.chat.into(),
                }
                .spec()
            })?;
            Ok(tx
                .notes()
                .snapshot(args.chat, args.id)?
                .ok_or_else(|| Aux::NotFound(format!("notebook snapshot {}", args.id)))?)
        })
        .await?;
    Ok(snapshot::Ok {
        snapshot: convert::note::snapshot(snapshot),
        notes: notes.into_iter().map(convert::note::from_db).collect(),
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Restore<C: Components>(
    state: &mut State<C>,
    args: restore::Args,
) -> Response<restore::Ok, restore::Err> {
    Policy::new(&state.caller).notes().write()?;
    valid_snapshot(args.id)?;
    edit(state, args.chat, move |tx, cfg, ctx| {
        let (backup, fill, events) = tools::restore(tx, cfg, ctx, args.id)?;
        Ok((
            restore::Ok {
                restored: args.id,
                backup,
                fill,
            },
            events,
        ))
    })
    .await
}
