//! Хендлеры `chats.*` (`05 §6`, `§8`, `11 §3`): права решает surface чата,
//! `Service` правит только поля `11 §3`; жизненный цикл — только хозяину.

use eva_protocol::{
    errors::{self, Aux, AuxResult, ext::*},
    requests::{
        Patch, Response,
        chats::{
            compact, create, delete, get, history_read, ingest, list, r#move, notify, restore,
            search, update,
        },
    },
    types::{
        ChatId,
        chat::{ExternalKind, Kind, Selector},
        folder,
        turn::TurnView,
    },
    wire::events::{ChatEvent, ChatPurged, Event},
};

use crate::{
    components::Components,
    config::Config,
    convert,
    db::{
        Db as _, Queries as _,
        chats::{ChatFilter, ChatRef, ChatRow, NewChat},
        folders::UNSORTED,
        stats::Metric,
        surfaces::SurfaceRef,
    },
    handling::folders::check_compact_at,
    shared::Shared,
    state::{Policy, State},
};

fn check_title(title: Option<&str>) -> AuxResult<()> {
    let ok = title.is_none_or(|t| !t.trim().is_empty() && !t.chars().any(char::is_control));
    ok.true_or(Aux::Invalid(
        "chat title must be a non-empty single line".into(),
    ))
}

/// `workdir_on` — имя из `hands.endpoints` (`05 §1.3`).
fn check_endpoint(cfg: &Config, workdir_on: Option<&str>) -> AuxResult<()> {
    match workdir_on {
        Some(name) if !cfg.hands.endpoints.contains_key(name) => Err(Aux::Invalid(format!(
            "workdir_on {name:?} is not in hands.endpoints"
        ))),
        _ => Ok(()),
    }
}

fn not_found<E: From<errors::chats::NotFound>>(chat: impl Into<Selector>) -> errors::Generic<E> {
    errors::chats::NotFound { chat: chat.into() }.spec()
}

fn internal_vanished(chat: ChatId) -> Aux {
    Aux::internal(format!("chat {chat} vanished inside its transaction"))
}

fn publish_chat<C: Components>(shared: &Shared<C>, event: fn(ChatEvent) -> Event, chat: &ChatRow) {
    let payload = ChatEvent {
        chat: chat.id,
        surface: chat.surface.id.clone(),
        external_id: chat.alias.as_ref().map(|a| a.external_id.clone()),
    };
    shared
        .bus()
        .publish(event(payload), Some(chat.surface.clone()));
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Ingest<C: Components>(
    state: &mut State<C>,
    args: ingest::Args,
) -> Response<ingest::Ok, ingest::Err> {
    let _ = (state, args);
    Err(Aux::Unavailable("M2_PENDING: chats.ingest arrives with M2: turn engine".into()).into())
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Create<C: Components>(
    state: &mut State<C>,
    args: create::Args,
) -> Response<create::Ok, create::Err> {
    let State {
        shared,
        world,
        caller,
        ..
    } = state;
    let create::Args {
        surface,
        folder,
        external_id,
        external_kind,
        kind,
        title,
        project,
        workdir,
        workdir_on,
    } = args;

    match (&external_id, external_kind) {
        (None, Some(_)) => {
            return Err(Aux::Invalid("external_kind is given without external_id".into()).into());
        }
        (Some(id), _) if id.is_empty() => {
            return Err(Aux::Invalid("external_id must not be empty".into()).into());
        }
        _ => {}
    }
    check_title(title.as_deref())?;
    check_endpoint(shared.cfg(), workdir_on.as_deref())?;

    let kind = kind.unwrap_or(shared.cfg().chats.default_kind);
    let now = world.now();
    let who = caller.clone();
    let row = shared
        .db()
        .write(move |tx| -> Response<ChatRow, create::Err> {
            let policy = Policy::new(&who);
            let target = tx
                .surfaces()
                .get(&surface)?
                .ok_or_else(|| Aux::NotFound(format!("surface {surface}")))?;
            let target = target.surface_ref();
            policy.surfaces().own(&target)?;

            let unsorted = tx.folders().by_name(&surface, UNSORTED)?.ok_or_else(|| {
                Aux::internal(format!("surface {surface} has no {UNSORTED} folder"))
            })?;
            let folder_ref = match folder {
                None => unsorted.folder_ref(&unsorted),
                Some(id) => {
                    let row = tx
                        .folders()
                        .get(id)?
                        .ok_or_else(|| Aux::NotFound(format!("folder {id}")))?;
                    (row.surface.id == surface).true_or(Aux::Invalid(format!(
                        "folder {id} is not on surface {surface}"
                    )))?;
                    row.folder_ref(&unsorted)
                }
            };
            policy.chats().create(&target, &folder_ref)?;

            if let Some(external) = &external_id
                && let Some(owner) = tx.chats().alias_owner(&surface, external)?
            {
                return Err(Aux::Conflict(format!(
                    "external chat {external} of surface {surface} is already chat {owner}"
                ))
                .into());
            }

            let id = tx.chats().insert(&NewChat {
                folder_id: folder_ref.id,
                kind,
                title,
                project,
                workdir,
                workdir_on,
                now,
            })?;
            if let Some(external) = &external_id {
                tx.chats().insert_alias(
                    id,
                    &surface,
                    external,
                    external_kind.unwrap_or(ExternalKind::Direct),
                    now,
                )?;
            }
            let row = tx.chats().get(id)?.ok_or_else(|| internal_vanished(id))?;
            tx.chats().index_title(id, row.title.as_deref())?;
            Ok(row)
        })
        .await?;

    publish_chat(shared, Event::ChatCreated, &row);
    log::info!(chat:% = row.id, surface:% = row.surface.id, caller = caller.label(); "chat created");
    Ok(create::Ok {
        chat: convert::chat::from_db(row),
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Get<C: Components>(
    state: &mut State<C>,
    args: get::Args,
) -> Response<get::Ok, get::Err> {
    let State {
        shared,
        caller,
        db_conn,
        ..
    } = state;
    let get::Args { chat } = args;
    let policy = Policy::new(caller);

    let db = db_conn.acquire(shared).await?;
    let selector = chat.clone();
    let row = db
        .run(move |c| match &selector {
            Selector::Id(id) => c.chats().get(*id),
            Selector::Alias {
                surface,
                external_id,
            } => c.chats().by_alias(surface, external_id),
        })
        .await?
        .ok_or_else(|| not_found(chat))?;
    policy.chats().read(&row.chat_ref())?;

    Ok(get::Ok {
        chat: convert::chat::from_db(row),
    })
}

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
    let list::Args {
        surface,
        folder,
        deleted,
        live,
        limit,
        after,
    } = args;
    let filter = ChatFilter {
        surfaces: Policy::new(caller).surfaces().visible(),
        surface,
        folder,
        deleted,
        live,
    };

    let db = db_conn.acquire(shared).await?;
    let rows = db
        .run(move |c| c.chats().list(&filter, after, limit.into_inner()))
        .await?
        .ok_or_else(|| {
            Aux::Conflict(format!(
                "page anchor {} is gone; list from the start",
                after.map_or_else(String::new, |a| a.to_string())
            ))
        })?;

    Ok(list::Ok {
        found: rows.into_iter().map(convert::chat::from_db).collect(),
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Update<C: Components>(
    state: &mut State<C>,
    args: update::Args,
) -> Response<update::Ok, update::Err> {
    let State { shared, caller, .. } = state;
    let update::Args { chat, update } = args;
    if let Patch::Change(workdir_on) = &update.workdir_on {
        check_endpoint(shared.cfg(), workdir_on.as_deref())?;
    }

    let who = caller.clone();
    let (row, reset) = shared
        .db()
        .write(move |tx| -> Response<(ChatRow, bool), update::Err> {
            let row = tx.chats().get(chat)?.ok_or_else(|| not_found(chat))?;
            Policy::new(&who).chats().mutate(&row.chat_ref(), &update)?;
            let before = row.trusted_chat;
            let title_before = row.title.clone();

            let row = apply(row, update)?;
            tx.chats().save(&row)?;
            if row.title != title_before {
                tx.chats().index_title(chat, row.title.as_deref())?;
            }

            let row = tx
                .chats()
                .get(chat)?
                .ok_or_else(|| internal_vanished(chat))?;
            // Hot-блок открытого сегмента собран под прежний trust (`05 §2.3`, `§2.4`).
            let reset = row.trusted_chat != before && tx.chats().mark_reset(chat)?;
            Ok((row, reset))
        })
        .await?;

    log::info!(chat:% = row.id, reset, caller = caller.label(); "chat updated");
    Ok(update::Ok {
        chat: convert::chat::from_db(row),
    })
}

/// Правка поверх загруженной строки; права уже решил `Policy`.
fn apply(mut row: ChatRow, update: update::Update) -> Response<ChatRow, update::Err> {
    let update::Update {
        title,
        pinned,
        alerted,
        kind,
        trusted,
        model,
        effort,
        thinking,
        compact_at,
        project,
        workdir,
        workdir_on,
    } = update;

    if let Patch::Change(title) = title {
        check_title(title.as_deref())?;
        row.title = title;
    }
    if let Patch::Change(kind) = kind {
        if row.kind == Kind::Persistent && kind == Kind::Ephemeral {
            return Err(errors::chats::KindMonotonic { chat: row.id }.spec());
        }
        row.kind = kind;
    }
    if let Patch::Change(pinned) = pinned {
        row.pinned = pinned;
        if pinned {
            row.kind = Kind::Persistent;
        }
    }
    if let Patch::Change(alerted) = alerted {
        row.alerted = alerted;
    }
    if let Patch::Change(trusted) = trusted {
        if row.agent {
            return Err(errors::chats::AgentTrust { chat: row.id }.spec());
        }
        row.trusted = trusted;
    }
    if let Patch::Change(model) = model {
        row.model = model;
    }
    if let Patch::Change(effort) = effort {
        row.effort = effort;
    }
    if let Patch::Change(thinking) = thinking {
        row.thinking = thinking;
    }
    if let Patch::Change(compact_at) = compact_at {
        check_compact_at(compact_at)?;
        row.compact_at = compact_at;
    }
    if let Patch::Change(project) = project {
        row.project = project;
    }
    if let Patch::Change(workdir) = workdir {
        row.workdir = workdir;
    }
    if let Patch::Change(workdir_on) = workdir_on {
        row.workdir_on = workdir_on;
    }

    Ok(row)
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Move<C: Components>(
    state: &mut State<C>,
    args: r#move::Args,
) -> Response<r#move::Ok, r#move::Err> {
    let State {
        shared,
        world,
        caller,
        ..
    } = state;
    Policy::new(caller).owner()?;
    let r#move::Args {
        chat,
        folder,
        confirm,
    } = args;
    let (now, zone) = (world.now(), shared.day_zone());

    let (row, reset) = shared
        .db()
        .write(move |tx| -> Response<(ChatRow, bool), r#move::Err> {
            let row = tx.chats().get(chat)?.ok_or_else(|| not_found(chat))?;
            let target = match &folder {
                folder::Selector::Id(id) => tx.folders().get(*id)?,
                folder::Selector::Name { surface, name } => tx.folders().by_name(surface, name)?,
            }
            .ok_or_else(|| Aux::NotFound(format!("folder {folder}")))?;
            if target.surface.id != row.surface.id {
                return Err(errors::chats::CrossSurface {
                    chat,
                    surface: row.surface.id,
                }
                .spec());
            }

            let from = row.trusted_chat;
            let to = row.trusted.unwrap_or_else(|| target.effective_trust());
            if from != to && !confirm {
                return Err(errors::chats::TrustChange { chat, from, to }.spec());
            }
            tx.chats().move_to(chat, target.id)?;
            if from != to {
                tx.stats().bump(Metric::ChatsTrustMoves, "", now, zone)?;
            }
            let reset = from != to && tx.chats().mark_reset(chat)?;
            let row = tx
                .chats()
                .get(chat)?
                .ok_or_else(|| internal_vanished(chat))?;
            Ok((row, reset))
        })
        .await?;

    log::info!(chat:% = row.id, folder:% = row.folder_id, reset, caller = caller.label(); "chat moved");
    Ok(r#move::Ok {
        chat: convert::chat::from_db(row),
    })
}

/// Что сделал `chats.delete`: событие публикуется после коммита.
enum Deleted {
    Soft(Box<ChatRow>),
    Purged(Vec<ChatRef>),
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Delete<C: Components>(
    state: &mut State<C>,
    args: delete::Args,
) -> Response<delete::Ok, delete::Err> {
    let State {
        shared,
        world,
        caller,
        ..
    } = state;
    Policy::new(caller).owner()?;
    let delete::Args { chat, purge } = args;

    let now = world.now();
    let deleted = shared
        .db()
        .write(move |tx| -> Response<Deleted, delete::Err> {
            let row = tx.chats().get(chat)?.ok_or_else(|| not_found(chat))?;
            if row.undeletable {
                return Err(errors::chats::Undeletable { chat }.spec());
            }
            if purge {
                let set = tx.chats().purge_set(chat)?;
                let ids: Vec<ChatId> = set.iter().map(|c| c.id).collect();
                tx.chats().purge(&ids)?;
                return Ok(Deleted::Purged(set));
            }
            if row.deleted_at.is_none() {
                tx.chats().set_deleted(chat, Some(now))?;
            }
            Ok(Deleted::Soft(Box::new(row)))
        })
        .await?;

    match deleted {
        Deleted::Soft(row) => {
            publish_chat(shared, Event::ChatDeleted, &row);
            log::info!(chat:% = chat, caller = caller.label(); "chat deleted");
        }
        Deleted::Purged(set) => {
            log::info!(chat:% = chat, purged = set.len(), caller = caller.label(); "chat purged");
            publish_purged(shared, set);
        }
    }
    Ok(delete::Ok {})
}

/// `chat.purged` — по событию на surface: `Service` видит только id своих (`06 §7`).
fn publish_purged<C: Components>(shared: &Shared<C>, set: Vec<ChatRef>) {
    let mut groups: Vec<(SurfaceRef, Vec<ChatId>)> = Vec::new();
    for ChatRef { id, surface } in set {
        match groups.iter_mut().find(|(s, _)| *s == surface) {
            Some((_, ids)) => ids.push(id),
            None => groups.push((surface, vec![id])),
        }
    }
    for (surface, ids) in groups {
        shared
            .bus()
            .publish(Event::ChatPurged(ChatPurged { ids }), Some(surface));
    }
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Restore<C: Components>(
    state: &mut State<C>,
    args: restore::Args,
) -> Response<restore::Ok, restore::Err> {
    let State { shared, caller, .. } = state;
    Policy::new(caller).owner()?;
    let restore::Args { chat } = args;

    let row = shared
        .db()
        .write(move |tx| -> Response<ChatRow, restore::Err> {
            let row = tx.chats().get(chat)?.ok_or_else(|| not_found(chat))?;
            if row.deleted_at.is_none() {
                return Err(errors::chats::NotDeleted { chat }.spec());
            }
            tx.chats().set_deleted(chat, None)?;
            Ok(tx
                .chats()
                .get(chat)?
                .ok_or_else(|| internal_vanished(chat))?)
        })
        .await?;

    publish_chat(shared, Event::ChatRestored, &row);
    log::info!(chat:% = row.id, caller = caller.label(); "chat restored");
    Ok(restore::Ok {
        chat: convert::chat::from_db(row),
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Search<C: Components>(
    state: &mut State<C>,
    args: search::Args,
) -> Response<search::Ok, search::Err> {
    Policy::new(&state.caller).owner()?;
    let _ = args;
    Err(
        Aux::Unavailable("M3_PENDING: chats.search arrives with M3: chat search index".into())
            .into(),
    )
}

/// Гейт видимости тура (`05 §7.3`): вне `master ∧ trusted` — текущий чат и
/// неудалённые untrusted.
fn in_view(view: &TurnView, chat: &ChatRow) -> bool {
    !view.public() || chat.id == view.chat || (!chat.trusted_chat && chat.deleted_at.is_none())
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn HistoryRead<C: Components>(
    state: &mut State<C>,
    args: history_read::Args,
) -> Response<history_read::Ok, history_read::Err> {
    let State {
        shared,
        caller,
        db_conn,
        ..
    } = state;
    let history_read::Args {
        chat,
        from_seq,
        count,
        view,
    } = args;
    let policy = Policy::new(caller);

    let db = db_conn.acquire(shared).await?;
    let row = db
        .run(move |c| c.chats().get(chat))
        .await?
        .ok_or_else(|| not_found(chat))?;
    policy.chats().read(&row.chat_ref())?;
    if let Some(view) = &view {
        in_view(view, &row).true_or(Aux::forbidden(format_args!(
            "chat {chat} is outside the scope of this turn"
        )))?;
    }

    let count = count.into_inner();
    let mut rows = db
        .run(move |c| c.chats().messages(chat, from_seq, count + 1))
        .await?;
    let next_seq = if rows.len() > usize::from(count) {
        rows.pop().map(|m| m.seq)
    } else {
        None
    };

    Ok(history_read::Ok {
        messages: rows
            .into_iter()
            .map(convert::chat::message_from_db)
            .collect(),
        next_seq,
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Compact<C: Components>(
    state: &mut State<C>,
    args: compact::Args,
) -> Response<compact::Ok, compact::Err> {
    Policy::new(&state.caller).owner()?;
    let _ = args;
    Err(Aux::Unavailable("M2_PENDING: chats.compact arrives with M2: turn engine".into()).into())
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Notify<C: Components>(
    state: &mut State<C>,
    args: notify::Args,
) -> Response<notify::Ok, notify::Err> {
    Policy::new(&state.caller).kernel()?;
    let _ = args;
    Err(Aux::Unavailable("M2_PENDING: chats.notify arrives with M2: turn engine".into()).into())
}
