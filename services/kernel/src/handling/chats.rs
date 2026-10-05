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
    handling::{folders::check_compact_at, text},
    shared::Shared,
    state::{Policy, State},
};

fn check_title(title: Option<&str>) -> AuxResult<()> {
    let Some(title) = title else {
        return Ok(());
    };
    let ok = !title.trim().is_empty() && !title.chars().any(char::is_control);
    ok.true_or(Aux::Invalid(
        "chat title must be a non-empty single line".into(),
    ))?;
    text::chars("chat title", title, text::NAME_MAX)
}

/// `project` и `workdir` — пути, не документы.
fn check_paths(project: Option<&str>, workdir: Option<&str>) -> AuxResult<()> {
    for (what, value) in [("project", project), ("workdir", workdir)] {
        if let Some(value) = value {
            text::bytes(what, value, text::PATH_MAX)?;
        }
    }
    Ok(())
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
    use crate::subsystems::turns::{ctx, ingest as input};
    let surface = args.surface.clone();
    let who = state.caller.clone();
    state
        .shared
        .db()
        .reader()
        .await?
        .run(move |c| {
            Policy::new(&who).surfaces().named(
                format_args!("surface {surface}"),
                c.surfaces().get(&surface)?.map(|s| s.surface_ref()),
            )
        })
        .await?;
    if args.external_chat.id.is_empty() {
        return Err(Aux::Invalid("external chat id must not be empty".into()).into());
    }
    text::bytes(
        "external chat id",
        &args.external_chat.id,
        text::EXTERNAL_ID_MAX,
    )?;
    if let Some(title) = &args.external_chat.title {
        text::chars("external chat title", title, text::NAME_MAX)?;
    }
    crate::handling::turns::validate_content(state, &args.content).await?;
    let actor = input::actor(state, args.surface.clone(), args.sender).await?;
    let (surface, external, now) = (args.surface.clone(), args.external_chat, state.shared.now());
    let kind = if actor.kind == eva_protocol::types::turn::ActorKind::Unknown {
        Kind::Ephemeral
    } else {
        Kind::Persistent
    };
    let (chat, created) = state
        .shared
        .db()
        .write(move |tx| {
            let (chat, created) = if let Some(row) = tx.chats().by_alias(&surface, &external.id)? {
                if kind == Kind::Persistent {
                    tx.chats().promote(row.id)?;
                }
                (row.id, false)
            } else {
                let folder = tx
                    .folders()
                    .by_name(&surface, UNSORTED)?
                    .ok_or_else(|| Aux::internal("surface has no Unsorted folder"))?;
                let id = tx.chats().insert(&NewChat {
                    folder_id: folder.id,
                    kind,
                    title: None,
                    project: None,
                    workdir: None,
                    workdir_on: None,
                    now,
                })?;
                tx.chats()
                    .insert_alias(id, &surface, &external.id, external.kind, now)?;
                (id, true)
            };
            let title = external.title.map(|s| crate::markers::sanitize_inline(&s));
            tx.chats().set_external_title(chat, title.as_deref())?;
            Result::<_, Aux>::Ok((chat, created))
        })
        .await?;
    if created
        && let Some(row) = state
            .shared
            .db()
            .reader()
            .await?
            .run(move |c| c.chats().get(chat))
            .await?
    {
        publish_chat(&state.shared, Event::ChatCreated, &row);
    }
    let mut entry = input::entry(&state.shared, &state.caller, chat, actor);
    entry.anchor = args.anchor;
    if let Some(over) = args.r#override {
        entry.overrides = ctx::Overrides {
            model: over.model,
            effort: over.effort,
            thinking: over.thinking,
        };
    }
    let result = input::append(
        &state.shared,
        entry,
        input::Message {
            content: args.content,
            external_id: args.external_id,
            reply_to_external_id: args.reply_to_external_id,
            silent: args.silent,
            machine: false,
        },
    )
    .await?;
    Ok(ingest::Ok {
        chat,
        message_id: result.message_id,
        turn: result.turn,
    })
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
        (Some(id), _) => text::bytes("external_id", id, text::EXTERNAL_ID_MAX)?,
        _ => {}
    }
    check_title(title.as_deref())?;
    check_paths(project.as_deref(), workdir.as_deref())?;
    check_endpoint(shared.cfg(), workdir_on.as_deref())?;

    let kind = kind.unwrap_or(shared.cfg().chats.default_kind);
    let now = world.now();
    let who = caller.clone();
    let row = shared
        .db()
        .write(move |tx| -> Response<ChatRow, create::Err> {
            let policy = Policy::new(&who);
            let target = policy.surfaces().named(
                format_args!("surface {surface}"),
                tx.surfaces().get(&surface)?.map(|s| s.surface_ref()),
            )?;

            let unsorted = tx.folders().by_name(&surface, UNSORTED)?.ok_or_else(|| {
                Aux::internal(format!("surface {surface} has no {UNSORTED} folder"))
            })?;
            let folder_ref = match folder {
                None => unsorted.folder_ref(&unsorted),
                Some(id) => {
                    let row = policy
                        .surfaces()
                        .named(format_args!("folder {id}"), tx.folders().get(id)?)?;
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
    let (selector, who) = (chat.clone(), caller.clone());
    let row = db
        .run(move |c| -> Response<_, get::Err> {
            Ok(match &selector {
                Selector::Id(id) => c.chats().get(*id)?,
                Selector::Alias {
                    surface,
                    external_id,
                } => {
                    let owner = c.surfaces().get(surface)?.map(|s| s.surface_ref());
                    Policy::new(&who).chats().alias(surface, owner.as_ref())?;
                    c.chats().by_alias(surface, external_id)?
                }
            })
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
    let who = caller.clone();
    let rows = db
        .run(move |c| -> Response<_, list::Err> {
            // Явный фильтр — объект аргумента: чужой не превращается в пустую страницу.
            let surfaces = Policy::new(&who).surfaces();
            if let Some(id) = &filter.surface {
                let owner = c.surfaces().get(id)?.map(|s| s.surface_ref());
                surfaces.named(format_args!("surface {id}"), owner)?;
            }
            if let Some(id) = filter.folder {
                surfaces.named(format_args!("folder {id}"), c.folders().get(id)?)?;
            }
            Ok(c.chats().list(&filter, after, limit.into_inner())?)
        })
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
        check_paths(project.as_deref(), None)?;
        row.project = project;
    }
    if let Patch::Change(workdir) = workdir {
        check_paths(None, workdir.as_deref())?;
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
                tx.stats().bump(Metric::ChatsTrustMoves, "", now, &zone)?;
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
    Soft(Vec<ChatRow>),
    Purged(Vec<ChatRef>),
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Delete<C: Components>(
    state: &mut State<C>,
    args: delete::Args,
) -> Response<delete::Ok, delete::Err> {
    Policy::new(&state.caller).owner()?;
    let tool_chat = state.tool_chat;
    let shared = state.shared.clone();
    let delete::Args { chat, purge } = args;
    let (reply, response) = tokio::sync::oneshot::channel();
    // Задача владеет и транзакцией: отмена ожидания spawn_blocking не отменяет commit.
    state.shared.turns().spawn(async move {
        let mut reply = Some(reply);
        let result: Response<delete::Ok, delete::Err> = async {
            let mut pauses = vec![shared.turns().slot(chat).pause_ingest().await];
            let now = shared.now();
            let (rows, victims) = shared
                .db()
                .write(
                    move |tx| -> Response<(Vec<ChatRow>, Vec<ChatRef>), delete::Err> {
                        let row = tx.chats().get(chat)?.ok_or_else(|| not_found(chat))?;
                        if row.undeletable {
                            return Err(errors::chats::Undeletable { chat }.spec());
                        }
                        let victims = tx.chats().purge_set(chat)?;
                        let mut rows = Vec::with_capacity(victims.len());
                        for victim in &victims {
                            let row = tx
                                .chats()
                                .get(victim.id)?
                                .ok_or_else(|| internal_vanished(victim.id))?;
                            if row.undeletable {
                                return Err(errors::chats::Undeletable { chat: victim.id }.spec());
                            }
                            if row.deleted_at.is_none() {
                                tx.chats().set_deleted(victim.id, Some(now))?;
                            }
                            tx.agents().set_status(
                                victim.id,
                                eva_protocol::types::agent::Status::Stopped,
                                now,
                            )?;
                            if !purge {
                                rows.push(row);
                            }
                        }
                        Ok((rows, victims))
                    },
                )
                .await?;
            for victim in victims.iter().filter(|c| c.id != chat) {
                pauses.push(shared.turns().slot(victim.id).pause_ingest().await);
            }
            // A child append may have resolved before the first tombstone and
            // committed an auto-restore while we waited for its ingest gate.
            // With every victim paused, make deletion the final DB write.
            let paused_victims = victims.clone();
            shared
                .db()
                .write(move |tx| -> Response<(), delete::Err> {
                    for victim in &paused_victims {
                        if tx
                            .chats()
                            .get(victim.id)?
                            .is_some_and(|row| row.deleted_at.is_none())
                        {
                            tx.chats().set_deleted(victim.id, Some(now))?;
                        }
                        tx.agents().set_status(
                            victim.id,
                            eva_protocol::types::agent::Status::Stopped,
                            now,
                        )?;
                    }
                    Ok(())
                })
                .await?;
            let running: Vec<_> = victims
                .iter()
                .filter_map(|c| shared.turns().get(c.id).and_then(|s| s.live()))
                .collect();
            for live in &running {
                live.cancel(crate::subsystems::turns::log::EndedBy::ChatDeleted);
            }
            if tool_chat.is_some_and(|id| victims.iter().any(|c| c.id == id))
                && let Some(reply) = reply.take()
            {
                let _ = reply.send(Ok(delete::Ok {}));
            }
            for live in running {
                live.closed.cancelled().await;
            }
            for victim in &victims {
                shared.access().forget(victim.id);
            }
            let deleted = if purge {
                shared
                    .db()
                    .write(move |tx| -> Response<Deleted, delete::Err> {
                        let ids: Vec<_> = victims.iter().map(|c| c.id).collect();
                        tx.chats().purge(&ids)?;
                        Ok(Deleted::Purged(victims))
                    })
                    .await?
            } else {
                Deleted::Soft(rows)
            };
            match deleted {
                Deleted::Soft(rows) => {
                    for row in rows {
                        publish_chat(&shared, Event::ChatDeleted, &row);
                    }
                }
                Deleted::Purged(set) => publish_purged(&shared, set),
            }
            Ok(delete::Ok {})
        }
        .await;
        if let Err(error) = &result {
            log::error!(chat:% = chat, error:% = error; "chat deletion failed");
        }
        if let Some(reply) = reply {
            let _ = reply.send(result);
        }
    });
    response
        .await
        .map_err(|_| Aux::Internal("chat deletion task stopped".into()))?
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

    let slot = shared.turns().slot(chat);
    let _gate = slot.gate.lock().await;
    if slot.ingest_paused() {
        return Err(Aux::Unavailable("chat is being deleted".into()).into());
    }

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
    let State {
        shared,
        caller,
        db_conn,
        ..
    } = state;
    Policy::new(caller).owner()?;
    let db = db_conn.acquire(shared).await?;
    let hits = crate::subsystems::search::query::search(shared, db, args).await?;
    Ok(search::Ok { hits })
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
        before,
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
        .run(move |c| match before {
            Some(before) => c.chats().messages_before(chat, before, count + 1),
            None => c.chats().messages(chat, from_seq, count + 1),
        })
        .await?;
    let next_seq = if rows.len() > usize::from(count) {
        let more = rows.pop().map(|m| m.seq);
        if before.is_some() {
            rows.last().map(|m| m.seq)
        } else {
            more
        }
    } else {
        None
    };
    if before.is_some() {
        rows.reverse();
    }

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
    use crate::subsystems::turns::{compaction, ctx};
    crate::handling::turns::chat::<C, compact::Err>(state, args.chat).await?;
    let slot = state.shared.turns().slot(args.chat);
    let _gate = slot.gate.lock().await;
    if slot.ingest_paused() {
        return Err(Aux::Unavailable("chat is being deleted".into()).into());
    }
    if slot.live().is_some() {
        return Err(Aux::Conflict("turn in progress".into()).into());
    }
    slot.keep_alive(None);
    let entry = ctx::Entry::master(args.chat, eva_protocol::types::PersonId::system());
    let llm = state.shared.cfg().llm.clone();
    let context = state
        .shared
        .db()
        .reader()
        .await?
        .run(move |c| ctx::resolve(c, entry, &llm))
        .await?;
    let id = compaction::manual(&state.shared, &context).await?;
    let row = state
        .shared
        .db()
        .reader()
        .await?
        .run(move |c| c.turns().segment(id))
        .await?
        .ok_or_else(|| Aux::internal("compacted segment vanished"))?;
    Ok(compact::Ok {
        segment: convert::turn::segment_from_db(row),
    })
}

#[eva::endpoint]
#[fastrace::trace]
pub async fn Notify<C: Components>(
    state: &mut State<C>,
    args: notify::Args,
) -> Response<notify::Ok, notify::Err> {
    Policy::new(&state.caller).kernel()?;
    use crate::subsystems::turns::{ctx, ingest as input};
    let subject = crate::markers::sanitize_inline_bounded(
        &args.subject,
        state.shared.cfg().chats.marker.name_tokens,
        state.shared.cfg().context.ratio_default,
    );
    check_title(Some(&subject))?;
    let now = state.shared.now();
    let chat = state
        .shared
        .db()
        .write(move |tx| {
            let id = tx.turns().notify_chat(now)?;
            if let Some(source) = args.source_chat {
                tx.turns().set_alerted(source)?;
            }
            Result::<_, Aux>::Ok(id)
        })
        .await?;
    let entry = ctx::Entry::master(chat, eva_protocol::types::PersonId::system());
    let text = format!(
        "[notify] {subject}\n{}",
        crate::subsystems::turns::marker::quote(&args.body)
    );
    let result = input::append(
        &state.shared,
        entry,
        input::Message {
            content: vec![eva_protocol::types::content::Block::text(text)],
            external_id: None,
            reply_to_external_id: None,
            silent: true,
            machine: true,
        },
    )
    .await?;
    state.shared.bus().publish(
        Event::Notify(eva_protocol::wire::events::NotifyEvent {
            subject,
            chat_id: chat,
            source_chat: args.source_chat,
        }),
        None,
    );
    state.shared.bump(Metric::NotifySent, "").await;
    Ok(notify::Ok {
        message_id: result.message_id,
    })
}
