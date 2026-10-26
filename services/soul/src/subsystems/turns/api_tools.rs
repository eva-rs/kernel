//! Инструменты чатов и личностей вызывают те же операции, что и клиент.

use std::{collections::BTreeMap, fmt::Display};

use eva::{
    data,
    time::{HumanDuration, Timestamp},
};

use eva_protocol::{
    errors::{Aux, Generic},
    requests::{Chats, HistoryCount, Limit, Patch, Response, SearchLimit},
    types::{
        ChatId, FolderId, PersonId, SurfaceId,
        chat::{Chat, Kind, Role},
        content::Block,
        folder,
        tool::{ResultForm, ToolContent, ToolDecl},
    },
    wire::tools::ToolResult,
};
use serde_json::Value;

use super::{
    estimate::Estimator,
    log::{Event, Payload},
    marker,
    ports::ToolsetCall,
};
use crate::{
    components::Components,
    config::Config,
    db::{Db as _, DbResult, Queries as _},
    markers::sanitize_inline,
    shared::Shared,
    subsystems::tools::schema::input_schema,
};

#[eva::int(u8, 1..=100)]
pub enum ChatLimit {}
impl Default for ChatLimit {
    fn default() -> Self {
        Self::POS30
    }
}
#[eva::int(u8, 1..=100)]
pub enum PersonLimit {}
impl Default for PersonLimit {
    fn default() -> Self {
        Self::POS50
    }
}

#[data]
#[serde(deny_unknown_fields)]
pub struct Search {
    pub query: String,
    #[serde(default)]
    pub limit: SearchLimit,
    pub since: Option<HumanDuration>,
    pub surface: Option<SurfaceId>,
    pub folder: Option<String>,
    #[serde(default)]
    pub include_deleted: bool,
}
#[data]
#[serde(deny_unknown_fields)]
pub struct History {
    pub chat_id: Option<ChatId>,
    #[serde(default)]
    pub from_seq: u64,
    #[serde(default)]
    pub count: HistoryCount,
}
#[data]
#[serde(deny_unknown_fields)]
pub struct List {
    pub surface: Option<SurfaceId>,
    pub folder: Option<String>,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub limit: ChatLimit,
}
#[data]
#[serde(deny_unknown_fields)]
pub struct Alert {
    pub chat_id: Option<ChatId>,
    pub on: bool,
    pub reason: Option<String>,
}
#[data]
#[serde(deny_unknown_fields)]
pub struct Pin {
    pub chat_id: Option<ChatId>,
    pub on: bool,
}
#[data]
#[serde(deny_unknown_fields)]
pub struct Current {
    pub chat_id: Option<ChatId>,
}
#[data]
#[serde(deny_unknown_fields)]
pub struct Rename {
    pub chat_id: Option<ChatId>,
    pub title: String,
}
#[data]
#[serde(deny_unknown_fields)]
pub struct Move {
    pub chat_id: Option<ChatId>,
    pub folder: String,
    #[serde(default)]
    pub confirm: bool,
}
#[data]
#[serde(deny_unknown_fields)]
pub struct Delete {
    pub chat_id: Option<ChatId>,
    #[serde(default)]
    pub purge: bool,
}
#[data]
#[serde(deny_unknown_fields)]
pub struct Restore {
    pub chat_id: ChatId,
}
#[data]
#[serde(deny_unknown_fields)]
pub struct People {
    pub surface: Option<SurfaceId>,
    pub whitelisted: Option<bool>,
    #[serde(default)]
    pub limit: PersonLimit,
}
#[data]
#[serde(deny_unknown_fields)]
pub struct PersonRename {
    pub person_id: PersonId,
    pub name: String,
}
#[data]
#[serde(deny_unknown_fields)]
pub struct Merge {
    pub from: PersonId,
    pub into: PersonId,
    #[serde(default)]
    pub confirm: bool,
}
#[data]
#[serde(deny_unknown_fields)]
pub struct Whitelist {
    pub person_id: PersonId,
    pub on: bool,
}

fn decl<T: eva::_priv::schemars::JsonSchema>(
    path: &str,
    description: &str,
    master: bool,
    trusted: bool,
    read: bool,
) -> ToolDecl {
    ToolDecl {
        path: path.into(),
        description: description.into(),
        input: input_schema::<T>(),
        master_only: master,
        trusted_only: trusted,
        needs: vec![],
        ends_turn: false,
        parallel_safe: read,
        waits_on_cancel: false,
        result: if path == "chats.history.read" {
            ResultForm::Uncapped
        } else {
            ResultForm::Windowed
        },
        timeout: None,
        idempotent: false,
    }
}

pub fn decls() -> Vec<ToolDecl> {
    vec![
        decl::<Search>(
            "chats.search",
            "Search visible conversations by words or meaning. Optional since is a duration such as 7d.",
            false,
            false,
            true,
        ),
        decl::<History>(
            "chats.history.read",
            "Read messages with speaker markers, starting at from_seq. Continue at the returned cursor.",
            false,
            false,
            true,
        ),
        decl::<List>(
            "chats.list",
            "List conversations, optionally by surface, folder or deleted status.",
            true,
            true,
            true,
        ),
        decl::<Alert>(
            "chats.set_alert",
            "Flag a chat for the human's attention and send a notification, or clear its alert.",
            true,
            false,
            false,
        ),
        decl::<Pin>(
            "chats.pin",
            "Pin or unpin a chat; pinning also makes it persistent.",
            true,
            false,
            false,
        ),
        decl::<Current>(
            "chats.persist",
            "Keep a chat permanently instead of expiring it as ephemeral.",
            true,
            false,
            false,
        ),
        decl::<Rename>(
            "chats.rename",
            "Set a chat title without changing its external title.",
            true,
            false,
            false,
        ),
        decl::<Move>(
            "chats.move",
            "Move a chat to an existing folder on the same surface; confirm a trust change explicitly.",
            true,
            false,
            false,
        ),
        decl::<Delete>(
            "chats.delete",
            "Delete a chat, or purge its history and subagents. Deleting this chat cancels the current turn.",
            true,
            false,
            false,
        ),
        decl::<Restore>(
            "chats.restore",
            "Restore a deleted chat.",
            true,
            false,
            false,
        ),
        decl::<People>(
            "persons.list",
            "List people and their identities, optionally by surface or whitelist status.",
            true,
            true,
            true,
        ),
        decl::<PersonRename>("persons.rename", "Rename a person.", true, true, false),
        decl::<Merge>(
            "persons.merge",
            "Merge identities and authorship into another person; confirm merging into the master explicitly.",
            true,
            true,
            false,
        ),
        decl::<Whitelist>(
            "persons.whitelist",
            "Allow or disallow a person to start turns; the master cannot be removed.",
            true,
            true,
            false,
        ),
    ]
}

fn parse<T: serde::de::DeserializeOwned>(input: Value) -> Result<T, Aux> {
    serde_json::from_value(input).map_err(|e| Aux::Invalid(e.to_string()))
}
fn api<T, E: Display>(result: Response<T, E>) -> Result<T, Aux> {
    result.map_err(|e| match e {
        Generic::Aux(e) => e,
        Generic::Spec(e) => Aux::Invalid(e.to_string()),
    })
}
fn text(text: impl Into<String>) -> ToolResult {
    ToolResult::ok(vec![ToolContent::text(text)])
}
fn inline(value: &str, cfg: &Config, ratio: Option<f64>) -> String {
    let est = Estimator::new(ratio, &cfg.context);
    crate::markers::sanitize_inline_bounded(value, cfg.chats.marker.name_tokens, est.ratio)
}
fn clip(text: &str, bytes: usize) -> &str {
    let mut end = bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
fn title(chat: &Chat) -> String {
    chat.external_title
        .as_ref()
        .or(chat.title.as_ref())
        .cloned()
        .unwrap_or_else(|| format!("{}", chat.id))
}

async fn folder_id<C: Components>(
    shared: &Shared<C>,
    surface: SurfaceId,
    name: Option<String>,
) -> Result<Option<FolderId>, Aux> {
    let Some(name) = name else {
        return Ok(None);
    };
    shared
        .db()
        .reader()
        .await?
        .run(move |c| {
            c.folders()
                .by_name(&surface, &name)?
                .map(|f| Some(f.id))
                .ok_or_else(|| Aux::NotFound(format!("folder {surface}/{name}")))
        })
        .await
}

pub async fn call<C: Components>(
    shared: &Shared<C>,
    call: ToolsetCall<'_>,
) -> Result<ToolResult, Aux> {
    match invoke(shared, call).await {
        Ok(result) => Ok(text(result)),
        Err(e @ (Aux::Db(_) | Aux::Internal(_))) => Err(e),
        Err(e) => Ok(ToolResult::error(e.to_string())),
    }
}

async fn invoke<C: Components>(shared: &Shared<C>, call: ToolsetCall<'_>) -> Result<String, Aux> {
    let Some(decl) = decls()
        .into_iter()
        .find(|d| d.path.replace('.', "_") == call.canonical)
    else {
        return Err(Aux::Internal(format!(
            "unknown chat/person tool {}",
            call.canonical
        )));
    };
    if decl.master_only && !call.view.master_turn {
        return Err(Aux::forbidden("master_only"));
    }
    if decl.trusted_only && !call.view.trusted_chat {
        return Err(Aux::forbidden("trusted_only"));
    }
    let mut session = crate::session::for_tool(shared, call.ctx.chat);
    let cfg = shared.cfg();
    let clean = |s: &str| inline(s, cfg, call.ratio);
    let current = call.ctx.chat;
    match call.canonical {
        "chats_search" => {
            let args: Search = parse(call.input)?;
            let full = !call.view.public();
            let folder = if full {
                folder_id(
                    shared,
                    args.surface
                        .clone()
                        .unwrap_or_else(|| call.ctx.surface.clone()),
                    args.folder,
                )
                .await?
            } else {
                None
            };
            let since = args.since.map(|d| {
                Timestamp::from_secs(shared.now().as_secs().saturating_sub(d.0.as_secs()))
            });
            let hits = api(session
                .chats()
                .search(
                    args.query,
                    args.limit,
                    since,
                    if full { args.surface } else { None },
                    folder,
                    full && args.include_deleted,
                    Some(call.view.clone()),
                )
                .await)?
            .hits;
            let ids: Vec<_> = hits.iter().filter_map(|h| h.sender.clone()).collect();
            let people = shared
                .db()
                .reader()
                .await?
                .run(move |c| {
                    ids.into_iter()
                        .map(|id| Ok((id.clone(), c.persons().get(&id)?)))
                        .collect::<DbResult<BTreeMap<_, _>>>()
                })
                .await?;
            let lines: Vec<_> = hits
                .into_iter()
                .map(|h| {
                    let role = match h.role {
                        Some(Role::Assistant) => "Assistant",
                        Some(Role::User)
                            if h.sender
                                .as_ref()
                                .and_then(|id| people.get(id))
                                .and_then(Option::as_ref)
                                .is_some_and(|p| p.master) =>
                        {
                            "Master"
                        }
                        Some(Role::User) if h.sender.is_some() => "User",
                        _ => "Unknown",
                    };
                    let speaker = h
                        .sender
                        .as_ref()
                        .map(|id| {
                            format!(
                                "{role}:{id} [{}]",
                                people
                                    .get(id)
                                    .and_then(Option::as_ref)
                                    .map(|p| clean(&p.name))
                                    .unwrap_or_default()
                            )
                        })
                        .unwrap_or_else(|| role.into());
                    format!(
                        "{} «{}» {} {} {}: {}",
                        h.chat,
                        clean(h.title.as_deref().unwrap_or("")),
                        h.seq
                            .map(|s| format!("#{s}"))
                            .unwrap_or_else(|| "title".into()),
                        marker::clock(h.at, &shared.day_zone().tz(), true),
                        speaker,
                        sanitize_inline(&h.snippet)
                    )
                })
                .collect();
            Ok(if lines.is_empty() {
                "no matches".into()
            } else {
                lines.join("\n")
            })
        }
        "chats_history_read" => {
            let args: History = parse(call.input)?;
            let chat = args.chat_id.unwrap_or(current);
            let page = api(session
                .chats()
                .history_read(
                    chat,
                    args.from_seq,
                    None,
                    args.count,
                    Some(call.view.clone()),
                )
                .await)?;
            let ids: Vec<_> = page.messages.iter().map(|m| m.id).collect();
            let payloads = shared
                .db()
                .reader()
                .await?
                .run(move |c| {
                    ids.into_iter()
                        .map(|id| {
                            let row = c.turns().event(chat, id)?.ok_or_else(|| {
                                Aux::Internal(format!("history event {id} vanished"))
                            })?;
                            let event = Event::try_from(row).map_err(Aux::internal)?;
                            Ok((id, event.payload))
                        })
                        .collect::<Result<BTreeMap<_, _>, Aux>>()
                })
                .await?;
            let rows: Vec<_> = page
                .messages
                .iter()
                .map(|m| {
                    let payload = payloads.get(&m.id);
                    let head = match payload {
                        Some(Payload::Message(p)) if p.speaker.is_some() => marker::speaker_line(
                            p.speaker.as_ref().expect("checked"),
                            p.reply.as_ref(),
                            m.external_id.as_deref(),
                            m.created_at,
                            &shared.day_zone().tz(),
                            true,
                        ),
                        _ if m.role == Role::Assistant => format!(
                            "Assistant {}",
                            marker::clock(m.created_at, &shared.day_zone().tz(), true)
                        ),
                        _ => format!(
                            "[soul] {}",
                            marker::clock(m.created_at, &shared.day_zone().tz(), true)
                        ),
                    };
                    let blocks = m
                        .blocks
                        .iter()
                        .map(|b| match b {
                            Block::Text { text } => marker::quote(text),
                            Block::Image { blob, .. } => marker::image(blob, None),
                            Block::File { blob } => marker::file(blob, None),
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    (m.seq, format!("#{} {head}\n{blocks}", m.seq))
                })
                .collect();
            Ok(history_window(
                &rows,
                page.next_seq,
                cfg.chats.history.read_tokens,
                Estimator::new(call.ratio, &cfg.context),
            ))
        }
        "chats_list" => {
            let args: List = parse(call.input)?;
            let folder = folder_id(
                shared,
                args.surface
                    .clone()
                    .unwrap_or_else(|| call.ctx.surface.clone()),
                args.folder,
            )
            .await?;
            let chats = api(session
                .chats()
                .list(
                    args.surface,
                    folder,
                    args.deleted,
                    false,
                    Limit::new(u16::from(args.limit.into_inner()))
                        .ok_or_else(|| Aux::Internal("validated limit out of range".into()))?,
                    None,
                )
                .await)?
            .found;
            let ids: Vec<_> = chats.iter().map(|c| c.folder_id).collect();
            let folders = shared
                .db()
                .reader()
                .await?
                .run(move |c| {
                    ids.into_iter()
                        .map(|id| {
                            Ok((id, c.folders().get(id)?.map(|f| f.name).unwrap_or_default()))
                        })
                        .collect::<DbResult<BTreeMap<_, _>>>()
                })
                .await?;
            Ok(if chats.is_empty() {
                "no chats".into()
            } else {
                chats
                    .into_iter()
                    .map(|c| {
                        format!(
                            "{} {} {}/{} «{}»{}{}{} {}",
                            c.id,
                            match c.kind {
                                Kind::Persistent => "persistent",
                                Kind::Ephemeral => "ephemeral",
                            },
                            clean(c.surface_id.as_str()),
                            clean(folders.get(&c.folder_id).map(String::as_str).unwrap_or("")),
                            clean(&title(&c)),
                            if c.pinned { " [pinned]" } else { "" },
                            if c.alerted { " [alerted]" } else { "" },
                            if c.undeletable { " [undeletable]" } else { "" },
                            marker::clock(c.last_activity_at, &shared.day_zone().tz(), true)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        "chats_set_alert" => {
            let args: Alert = parse(call.input)?;
            let chat = args.chat_id.unwrap_or(current);
            api(session
                .chats()
                .update(
                    chat,
                    Chats::update::Update {
                        alerted: Patch::Change(args.on),
                        ..Default::default()
                    },
                )
                .await)?;
            if args.on {
                let subject = args
                    .reason
                    .as_deref()
                    .map(clean)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("chat {chat} flagged"));
                api(session
                    .chats()
                    .notify(subject, format!("Chat {chat} needs attention."), Some(chat))
                    .await)?;
            }
            Ok(format!("alerted = {}", args.on))
        }
        "chats_pin" => {
            let args: Pin = parse(call.input)?;
            api(session
                .chats()
                .update(
                    args.chat_id.unwrap_or(current),
                    Chats::update::Update {
                        pinned: Patch::Change(args.on),
                        ..Default::default()
                    },
                )
                .await)?;
            Ok(format!("pinned = {}", args.on))
        }
        "chats_persist" => {
            let args: Current = parse(call.input)?;
            api(session
                .chats()
                .update(
                    args.chat_id.unwrap_or(current),
                    Chats::update::Update {
                        kind: Patch::Change(Kind::Persistent),
                        ..Default::default()
                    },
                )
                .await)?;
            Ok("kind = persistent".into())
        }
        "chats_rename" => {
            let args: Rename = parse(call.input)?;
            let title = clean(&args.title);
            api(session
                .chats()
                .update(
                    args.chat_id.unwrap_or(current),
                    Chats::update::Update {
                        title: Patch::Change(Some(title.clone())),
                        ..Default::default()
                    },
                )
                .await)?;
            Ok(format!("title = {title}"))
        }
        "chats_move" => {
            let args: Move = parse(call.input)?;
            let chat = args.chat_id.unwrap_or(current);
            let row = api(session.chats().get(chat.into()).await)?.chat;
            api(session
                .chats()
                .r#move(
                    chat,
                    folder::Selector::Name {
                        surface: row.surface_id,
                        name: args.folder,
                    },
                    args.confirm,
                )
                .await)?;
            Ok("moved".into())
        }
        "chats_delete" => {
            let args: Delete = parse(call.input)?;
            api(session
                .chats()
                .delete(args.chat_id.unwrap_or(current), args.purge)
                .await)?;
            Ok("deleted".into())
        }
        "chats_restore" => {
            let args: Restore = parse(call.input)?;
            api(session.chats().restore(args.chat_id).await)?;
            Ok("restored".into())
        }
        "persons_list" => {
            let args: People = parse(call.input)?;
            let people = api(session
                .persons()
                .list(
                    args.surface,
                    args.whitelisted,
                    Limit::new(u16::from(args.limit.into_inner()))
                        .ok_or_else(|| Aux::Internal("validated limit out of range".into()))?,
                    None,
                )
                .await)?
            .found;
            Ok(if people.is_empty() {
                "no people".into()
            } else {
                people
                    .into_iter()
                    .map(|p| {
                        let identities = p
                            .identities
                            .iter()
                            .map(|i| {
                                format!(
                                    "{}:{}",
                                    clean(i.surface.as_str()),
                                    clean(i.handle.as_deref().unwrap_or(&i.external_id))
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!(
                            "{} {}{}{} {}",
                            p.id,
                            clean(&p.name),
                            if p.master { " [master]" } else { "" },
                            if p.whitelisted { " [whitelisted]" } else { "" },
                            identities
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        "persons_rename" => {
            let args: PersonRename = parse(call.input)?;
            api(session.persons().rename(args.person_id, args.name).await)?;
            Ok("renamed".into())
        }
        "persons_merge" => {
            let args: Merge = parse(call.input)?;
            api(session
                .persons()
                .merge(args.from, args.into, args.confirm)
                .await)?;
            Ok("merged".into())
        }
        "persons_whitelist" => {
            let args: Whitelist = parse(call.input)?;
            api(session.persons().whitelist(args.person_id, args.on).await)?;
            Ok(format!("whitelisted = {}", args.on))
        }
        _ => Err(Aux::Internal("unhandled chat/person tool".into())),
    }
}

fn history_window(
    rows: &[(u64, String)],
    next: Option<u64>,
    tokens: u32,
    est: Estimator,
) -> String {
    if rows.is_empty() {
        return "no messages".into();
    }
    let bytes = (f64::from(tokens) * est.ratio) as usize;
    let available = bytes.saturating_sub(80);
    let mut out = String::new();
    let mut next = next;
    for (index, (seq, text)) in rows.iter().enumerate() {
        if out.len().saturating_add(text.len()).saturating_add(2) > available {
            if out.is_empty() {
                out.push_str(clip(text, available.saturating_sub(24)));
                out.push_str("\n[message truncated]");
                next = rows.get(index + 1).map(|r| r.0).or(next);
            } else {
                next = Some(*seq);
            }
            break;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(text);
    }
    if let Some(next) = next {
        out.push_str(&format!("\nread on with from_seq={next}"));
    }
    out
}
