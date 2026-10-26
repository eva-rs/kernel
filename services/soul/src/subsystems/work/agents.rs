//! Подагенты используют обычные чаты, туры и устойчивую почту.

use std::{collections::BTreeSet, time::Duration};

use eva::{data, time::HumanDuration};
use eva_protocol::{
    errors::Aux,
    types::{
        ChatId, EventId, ModelId, PersonId, SegmentId, ToolId, TurnId,
        agent::Status,
        chat::{Kind, Role},
        content::Block,
        tool::{Need, ResultForm, ToolContent, ToolDecl},
        turn::{ActorKind, FeedEvent, ItemBody, ItemStatus, PendingKind, TurnOutcome, TurnState},
    },
    wire::tools::ToolResult,
};
use serde_json::{Value, json};

use crate::{
    components::Components,
    config::Config,
    db::{
        Db as _, DbResult, Queries as _, agents::AgentRow, chats::NewChat, stats::Metric,
        turns::NewMessage,
    },
    markers::sanitize_inline,
    shared::Shared,
    subsystems::{
        stats::Origin,
        tools::{expose::Whitelist, schema::input_schema},
        turns::{
            ctx::Entry,
            feed,
            ingest::{self, Message},
            items,
            log::{self, Append, EndedBy, MessagePayload, Payload},
            ports::{self, BoxFuture, ToolsetCall},
        },
    },
};

#[data]
pub struct Spawn {
    pub name: String,
    pub task: String,
    pub model: Option<ModelId>,
    pub tools: Option<Vec<String>>,
    pub report_schema: Option<Value>,
}
#[data]
pub struct Send {
    pub agent: String,
    pub body: String,
}
#[data]
pub struct Wait {
    pub agent: Option<String>,
    pub timeout: Option<HumanDuration>,
}
#[data]
pub struct Stop {
    pub agent: String,
}
#[data]
pub struct Say {
    pub body: String,
}
#[data]
pub struct Done {
    pub report: Value,
}
#[data]
pub struct List {}

pub fn decls(cfg: &Config) -> Vec<ToolDecl> {
    [
        (
            "agents.spawn",
            "Start a subagent on a task with an explicit tool whitelist.",
            input_schema::<Spawn>(),
            vec![Need::Lead],
        ),
        (
            "agents.send",
            "Send a letter to a live subagent.",
            input_schema::<Send>(),
            vec![Need::Lead, Need::Team],
        ),
        (
            "agents.wait",
            "Read subagent letters or wait for progress, completion or permission.",
            input_schema::<Wait>(),
            vec![Need::Lead, Need::Team],
        ),
        (
            "agents.list",
            "List this chat's subagents and unread letter counts.",
            input_schema::<List>(),
            vec![Need::Lead],
        ),
        (
            "agents.stop",
            "Stop a live subagent.",
            input_schema::<Stop>(),
            vec![Need::Lead, Need::Team],
        ),
        (
            "agent.say",
            "Send a progress letter to your lead.",
            input_schema::<Say>(),
            vec![Need::Agent],
        ),
        (
            "agent.done",
            "Send your final report to your lead and finish your task.",
            input_schema::<Done>(),
            vec![Need::Agent],
        ),
    ]
    .into_iter()
    .map(|(path, description, mut input, needs)| {
        if path == "agent.done" {
            input["properties"]["report"] =
                json!({"anyOf":[{"type":"string"},{"type":"object","additionalProperties":true}]});
        }
        ToolDecl {
            path: path.into(),
            description: description.into(),
            input,
            needs,
            master_only: path.starts_with("agents."),
            trusted_only: false,
            ends_turn: path == "agent.done",
            parallel_safe: false,
            waits_on_cancel: false,
            result: ResultForm::Windowed,
            timeout: (path == "agents.wait").then_some(cfg.work.wait_max),
            idempotent: false,
        }
    })
    .collect()
}

fn success(value: Value) -> ToolResult {
    ToolResult::ok(vec![ToolContent::text(value.to_string())])
}
fn invalid(error: impl std::fmt::Display) -> Aux {
    Aux::Invalid(error.to_string())
}
fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, Aux> {
    serde_json::from_value(value).map_err(|e| invalid(format!("invalid input: {e}")))
}
fn live(status: Status) -> bool {
    matches!(status, Status::Running | Status::Idle)
}
fn slug(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'-'))
}
fn quote(body: &str) -> String {
    crate::subsystems::turns::marker::quote(body)
}
fn letter(agent: &AgentRow, body: &str, to_parent: bool) -> String {
    let header = if to_parent {
        format!(
            "[team] from \"{}\" (agent c{})",
            sanitize_inline(&agent.name),
            agent.chat.0
        )
    } else {
        format!("[team] from lead c{}", agent.parent.0)
    };
    format!("{header}\n{}", quote(body))
}

pub async fn call<C: Components>(
    shared: &Shared<C>,
    call: ToolsetCall<'_>,
) -> Result<ToolResult, Aux> {
    let result = async {
        match call.canonical {
            "agents_spawn" => spawn(shared, &call, parse(call.input.clone())?).await,
            "agents_send" => send(shared, call.ctx.chat, parse(call.input)?).await,
            "agents_wait" => wait(shared, call.ctx.chat, parse(call.input)?).await,
            "agents_list" => list(shared, call.ctx.chat).await,
            "agents_stop" => stop(shared, call.ctx.chat, parse(call.input)?).await,
            "agent_say" => say(shared, call.ctx.chat, parse(call.input)?).await,
            "agent_done" => done(shared, call.ctx.chat, parse(call.input)?).await,
            _ => Err(Aux::Internal(format!(
                "unknown agent tool {}",
                call.canonical
            ))),
        }
    }
    .await;
    match result {
        Err(Aux::Invalid(message)) => Ok(ToolResult::error(message)),
        other => other,
    }
}

async fn children<C: Components>(shared: &Shared<C>, parent: ChatId) -> Result<Vec<AgentRow>, Aux> {
    Ok(shared
        .db()
        .reader()
        .await?
        .run(move |c| c.agents().list(Some(parent)))
        .await?)
}
async fn own<C: Components>(shared: &Shared<C>, chat: ChatId) -> Result<AgentRow, Aux> {
    shared
        .db()
        .reader()
        .await?
        .run(move |c| c.agents().get(chat))
        .await?
        .ok_or_else(|| invalid("this chat is not a subagent"))
}
async fn named<C: Components>(
    shared: &Shared<C>,
    parent: ChatId,
    name: &str,
) -> Result<AgentRow, Aux> {
    children(shared, parent)
        .await?
        .into_iter()
        .rev()
        .find(|a| a.name == name)
        .ok_or_else(|| invalid(format!("unknown agent {name}")))
}
fn entry(agent: &AgentRow) -> Entry {
    let mut entry = Entry::master(agent.chat, PersonId::system());
    if !agent.master_turn {
        entry.actor.kind = ActorKind::Whitelisted;
    }
    entry.whitelist = Whitelist::Restricted(agent.tools.iter().copied().collect());
    entry.source = Origin::Agent;
    entry
}

async fn spawn<C: Components>(
    shared: &Shared<C>,
    call: &ToolsetCall<'_>,
    args: Spawn,
) -> Result<ToolResult, Aux> {
    if !slug(&args.name) {
        return Err(invalid("agent name must match [a-z0-9_-]{1,32}"));
    }
    if args.task.trim().is_empty() {
        return Err(invalid("task must not be empty"));
    }
    if let Some(schema) = &args.report_schema {
        if !schema.is_object() {
            return Err(invalid("report_schema must be a JSON Schema object"));
        }
        jsonschema::validator_for(schema)
            .map_err(|e| invalid(format!("invalid report_schema: {e}")))?;
    }
    let catalog = shared.tools().get();
    let mut ids = BTreeSet::<ToolId>::new();
    for path in args
        .tools
        .as_ref()
        .unwrap_or(&shared.cfg().work.agent_default_tools)
    {
        let entries = catalog.resolve(path);
        if entries.is_empty() {
            return Err(invalid(format!("unknown tool or group {path}")));
        }
        ids.extend(entries.into_iter().map(|e| e.id));
    }
    for name in ["agent_say", "agent_done"] {
        let tool = catalog
            .by_canonical(name)
            .ok_or_else(|| Aux::Internal(format!("missing {name}")))?;
        ids.insert(tool.id);
    }
    let parent = call.ctx.chat;
    let model = match args
        .model
        .or_else(|| shared.turns().slot(parent).last_ctx().map(|c| c.model))
    {
        Some(model) => model,
        None => {
            let llm = shared.cfg().llm.clone();
            shared
                .db()
                .reader()
                .await?
                .run(move |c| {
                    crate::subsystems::turns::ctx::resolve(
                        c,
                        Entry::master(parent, PersonId::system()),
                        &llm,
                    )
                    .map(|c| c.model)
                })
                .await?
        }
    };
    if shared.models().get(&model).is_none() {
        return Err(invalid(format!("unknown model {model}")));
    }
    let (trusted, master_turn, now, max) = (
        call.ctx.trusted_chat,
        call.ctx.master_turn,
        shared.now(),
        shared.cfg().work.max_agents,
    );
    let parent_gate = mutation_gate(shared, parent).await?;
    let (agent, mail, parent_gate) = shared
        .db()
        .write(move |tx| {
            let parent_row = tx
                .chats()
                .get(parent)?
                .ok_or_else(|| invalid("lead chat no longer exists"))?;
            if parent_row.deleted_at.is_some() {
                return Err(invalid("lead chat was deleted"));
            }
            if parent_row.agent {
                return Err(invalid("subagents cannot spawn subagents"));
            }
            let agents = tx.agents().list(Some(parent))?;
            if agents.iter().filter(|a| live(a.status)).count() >= max as usize {
                return Err(invalid(format!("max_agents limit {max} reached")));
            }
            if agents.iter().any(|a| a.name == args.name && live(a.status)) {
                return Err(invalid("agent name is already in use"));
            }
            let folder = tx
                .folders()
                .by_name(&parent_row.surface.id, "agents")?
                .ok_or_else(|| Aux::Internal("agents folder is missing".into()))?;
            let chat = tx.chats().insert(&NewChat {
                folder_id: folder.id,
                kind: Kind::Persistent,
                title: Some(format!("agent: {}", sanitize_inline(&args.name))),
                project: parent_row.project,
                workdir: parent_row.workdir,
                workdir_on: parent_row.workdir_on,
                now,
            })?;
            let mut row = tx
                .chats()
                .get(chat)?
                .ok_or_else(|| Aux::Internal("new agent chat is missing".into()))?;
            row.model = Some(model);
            row.trusted = Some(trusted);
            row.effort = parent_row.effort;
            row.thinking = parent_row.thinking;
            tx.chats().save(&row)?;
            tx.chats().index_title(chat, row.title.as_deref())?;
            let agent = AgentRow {
                chat,
                parent,
                name: args.name,
                task: args.task,
                tools: ids.into_iter().collect(),
                report_schema: args.report_schema,
                master_turn,
                status: Status::Running,
                unread: 0,
                started_at: now,
                finished_at: None,
            };
            tx.agents().insert(&agent)?;
            let mail = tx.agents().letter(chat, false, &agent.task, now)?;
            // Guard возвращается из write и живёт до commit даже при отмене future.
            Result::<_, Aux>::Ok((agent, mail, parent_gate))
        })
        .await?;
    drop(parent_gate);
    deliver(shared, &agent, mail, &agent.task).await?;
    Ok(success(
        json!({"agent": agent.name, "chat_id": agent.chat, "status": "running"}),
    ))
}

async fn mutation_gate<C: Components>(
    shared: &Shared<C>,
    chat: ChatId,
) -> Result<tokio::sync::OwnedMutexGuard<()>, Aux> {
    let slot = shared.turns().slot(chat);
    let gate = slot.gate.clone().lock_owned().await;
    if slot.ingest_paused() {
        return Err(Aux::Unavailable("chat is being deleted".into()));
    }
    Ok(gate)
}

async fn deliver<C: Components>(
    shared: &Shared<C>,
    agent: &AgentRow,
    mail: i64,
    body: &str,
) -> Result<(), Aux> {
    let message = Message {
        content: vec![Block::text(letter(agent, body, false))],
        external_id: None,
        reply_to_external_id: None,
        silent: false,
        machine: true,
    };
    if let Err(error) = Box::pin(ingest::append(shared, entry(agent), message)).await {
        if shared.turns().slot(agent.chat).ingest_paused() {
            return Err(error);
        }
        settle(
            shared,
            agent.clone(),
            Status::Failed,
            &format!("turn failed: {error}"),
            "",
        )
        .await?;
        return Err(error);
    }
    let _gate = mutation_gate(shared, agent.chat).await?;
    let now = shared.now();
    let (chat, running) = (agent.chat, shared.turns().live(agent.chat).is_some());
    let _gate = shared
        .db()
        .write(move |tx| {
            tx.agents().read_letters(&[mail], now)?;
            if running {
                tx.agents().set_status(chat, Status::Running, now)?;
            }
            DbResult::Ok(_gate)
        })
        .await?;
    shared.access().agents_changed.notify_waiters();
    Ok(())
}

async fn send<C: Components>(
    shared: &Shared<C>,
    parent: ChatId,
    args: Send,
) -> Result<ToolResult, Aux> {
    let agent = named(shared, parent, &args.agent).await?;
    if !live(agent.status) {
        return Err(invalid("agent finished"));
    }
    let chat = agent.chat;
    let gate = mutation_gate(shared, chat).await?;
    let running = shared.turns().live(chat).is_some();
    let now = shared.now();
    let body = args.body.clone();
    let (mail, gate) = shared
        .db()
        .write(move |tx| {
            if !tx.agents().set_status(chat, Status::Running, now)? {
                return Err(invalid("agent finished"));
            }
            Ok::<_, Aux>((tx.agents().letter(chat, false, &body, now)?, gate))
        })
        .await?;
    drop(gate);
    deliver(shared, &agent, mail, &args.body).await?;
    Ok(success(
        json!({"delivered": if running { "next_step" } else { "now" }}),
    ))
}

async fn say<C: Components>(
    shared: &Shared<C>,
    chat: ChatId,
    args: Say,
) -> Result<ToolResult, Aux> {
    let _gate = mutation_gate(shared, chat).await?;
    let _agent = own(shared, chat).await?;
    let now = shared.now();
    let _gate = shared
        .db()
        .write(move |tx| {
            let agent = tx
                .agents()
                .get(chat)?
                .ok_or_else(|| invalid("agent no longer exists"))?;
            if !live(agent.status) {
                return Err(invalid("agent finished"));
            }
            tx.agents().letter(chat, true, &args.body, now)?;
            Ok::<_, Aux>(_gate)
        })
        .await?;
    shared.access().agents_changed.notify_waiters();
    Ok(success(json!({"sent": true})))
}

async fn done<C: Components>(
    shared: &Shared<C>,
    chat: ChatId,
    args: Done,
) -> Result<ToolResult, Aux> {
    let _gate = mutation_gate(shared, chat).await?;
    let agent = own(shared, chat).await?;
    if !args.report.is_string() && !args.report.is_object() {
        return Err(invalid("report must be a string or object"));
    }
    if let Some(schema) = &agent.report_schema {
        let validator = jsonschema::validator_for(schema)
            .map_err(|e| invalid(format!("invalid report_schema: {e}")))?;
        if let Err(error) = validator.validate(&args.report) {
            return Err(invalid(format!(
                "report does not match report_schema: {error}"
            )));
        }
    }
    let body = args
        .report
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| args.report.to_string());
    let now = shared.now();
    let _gate = shared
        .db()
        .write(move |tx| {
            if !tx.agents().set_status(chat, Status::Done, now)? {
                return Err(invalid("agent finished"));
            }
            tx.agents().letter(chat, true, &body, now)?;
            Ok::<_, Aux>(_gate)
        })
        .await?;
    shared.bump(Metric::AgentsReported, "").await;
    shared.access().agents_changed.notify_waiters();
    Ok(success(json!({"sent": true, "status": "done"})))
}

async fn waiting<C: Components>(shared: &Shared<C>, chat: ChatId) -> Result<bool, Aux> {
    let now = shared.now();
    Ok(shared
        .db()
        .reader()
        .await?
        .run(move |c| c.agents().open_pending(chat))
        .await?
        .iter()
        .any(|p| p.kind == PendingKind::Permission && p.expires_at > now))
}

async fn list<C: Components>(shared: &Shared<C>, parent: ChatId) -> Result<ToolResult, Aux> {
    let mut rows = Vec::new();
    for agent in children(shared, parent).await? {
        rows.push(json!({"name": agent.name, "chat_id": agent.chat, "status": agent.status,
            "waiting": if waiting(shared, agent.chat).await? { "permission" } else { "none" },
            "unread": agent.unread, "started_at": agent.started_at, "finished_at": agent.finished_at}));
    }
    Ok(success(json!({"agents": rows})))
}

async fn wait<C: Components>(
    shared: &Shared<C>,
    parent: ChatId,
    args: Wait,
) -> Result<ToolResult, Aux> {
    let timeout = args.timeout.unwrap_or(shared.cfg().work.wait_default);
    if *timeout > *shared.cfg().work.wait_max {
        return Err(invalid("timeout exceeds work.wait_max"));
    }
    let deadline = tokio::time::Instant::now() + *timeout;
    let mut initial = None;
    loop {
        let changed = shared.access().agents_changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let agents = children(shared, parent).await?;
        let selected: Vec<_> = match &args.agent {
            Some(name) => agents
                .into_iter()
                .rev()
                .find(|a| &a.name == name)
                .into_iter()
                .collect(),
            None => agents,
        };
        if selected.is_empty() {
            return Err(invalid("unknown agent"));
        }
        let states: Vec<_> = selected.iter().map(|a| (a.chat, a.status)).collect();
        let statuses_changed = initial.as_ref().is_some_and(|old| old != &states);
        if initial.is_none() {
            initial = Some(states);
        }
        let mut result = Vec::new();
        let mut ready = statuses_changed || tokio::time::Instant::now() >= deadline;
        for agent in &selected {
            let permission = waiting(shared, agent.chat).await?;
            ready |= agent.unread > 0 || !live(agent.status) || permission;
            result.push(json!({"agent": agent.name, "status": agent.status,
                "waiting": if permission { "permission" } else { "none" }, "messages": []}));
        }
        if ready {
            let ids: BTreeSet<_> = selected.iter().map(|a| a.chat).collect();
            let now = shared.now();
            let messages = shared
                .db()
                .write(move |tx| {
                    let messages: Vec<_> = tx
                        .agents()
                        .unread(parent, true)?
                        .into_iter()
                        .filter(|m| ids.contains(&m.agent))
                        .collect();
                    tx.agents()
                        .read_letters(&messages.iter().map(|m| m.id).collect::<Vec<_>>(), now)?;
                    DbResult::Ok(messages)
                })
                .await?;
            for (agent, row) in selected.iter().zip(&mut result) {
                row["messages"] = json!(
                    messages
                        .iter()
                        .filter(|m| m.agent == agent.chat)
                        .map(|m| quote(&m.body))
                        .collect::<Vec<_>>()
                );
            }
            return Ok(success(if args.agent.is_some() {
                result.remove(0)
            } else {
                json!({"agents": result})
            }));
        }
        tokio::select! {
            () = &mut changed => {},
            () = tokio::time::sleep_until(deadline) => {},
            () = tokio::time::sleep(Duration::from_millis(100)) => {},
        }
    }
}

async fn stop<C: Components>(
    shared: &Shared<C>,
    parent: ChatId,
    args: Stop,
) -> Result<ToolResult, Aux> {
    let agent = named(shared, parent, &args.agent).await?;
    if !live(agent.status) {
        return Err(invalid("agent finished"));
    }
    let (chat, now) = (agent.chat, shared.now());
    if !shared
        .db()
        .write(move |tx| tx.agents().set_status(chat, Status::Stopped, now))
        .await?
    {
        return Err(invalid("agent finished"));
    }
    shared.turns().cancel(chat, EndedBy::Lead);
    shared.bump(Metric::AgentsStopped, "lead").await;
    let text = last_text(shared, chat).await?;
    notice(shared, &agent, Status::Stopped, "stopped by lead", &text).await?;
    Ok(success(json!({"agent": agent.name, "status": "stopped"})))
}

async fn notice<C: Components>(
    shared: &Shared<C>,
    agent: &AgentRow,
    status: Status,
    reason: &str,
    text: &str,
) -> Result<(), Aux> {
    shared.access().agents_changed.notify_waiters();
    let parent = agent.parent;
    if shared.turns().slot(parent).ingest_paused() {
        return Ok(());
    }
    let exists = shared
        .db()
        .reader()
        .await?
        .run(move |c| c.chats().get(parent))
        .await?;
    if exists.is_none_or(|c| c.deleted_at.is_some()) {
        return Ok(());
    }
    let status = serde_json::to_value(status).map_err(|e| Aux::Internal(e.to_string()))?;
    let status = status.as_str().unwrap_or("failed");
    let mut body = format!(
        "[team] agent \"{}\" {status}: {}",
        sanitize_inline(&agent.name),
        sanitize_inline(reason)
    );
    if !text.is_empty() {
        body.push('\n');
        body.push_str(&quote(text));
    }
    let mut entry = Entry::master(parent, PersonId::system());
    if !agent.master_turn {
        entry.actor.kind = ActorKind::Whitelisted;
    }
    let appended = Box::pin(ingest::append(
        shared,
        entry,
        Message {
            content: vec![Block::text(body)],
            external_id: None,
            reply_to_external_id: None,
            silent: false,
            machine: true,
        },
    ))
    .await?;
    if matches!(appended.turn, TurnState::Started(_)) {
        shared.bump(Metric::AgentsWokeLeader, "").await;
    }
    Ok(())
}

async fn settle<C: Components>(
    shared: &Shared<C>,
    agent: AgentRow,
    status: Status,
    reason: &str,
    text: &str,
) -> Result<(), Aux> {
    let (chat, now) = (agent.chat, shared.now());
    if !shared
        .db()
        .write(move |tx| tx.agents().set_status(chat, status, now))
        .await?
    {
        return Ok(());
    }
    let metric = match status {
        Status::Idle => Metric::AgentsSilent,
        Status::Stopped => Metric::AgentsStopped,
        _ => Metric::AgentsFailed,
    };
    shared
        .bump(
            metric,
            if reason == "interrupted by restart" {
                "restart"
            } else {
                "turn"
            },
        )
        .await;
    notice(shared, &agent, status, reason, text).await
}

pub async fn recover<C: Components>(shared: &Shared<C>) -> Result<(), Aux> {
    let agents = shared
        .db()
        .reader()
        .await?
        .run(|c| c.agents().list(None))
        .await?;
    for agent in agents {
        if agent.status == Status::Running && shared.turns().live(agent.chat).is_none() {
            let text = last_text(shared, agent.chat).await?;
            settle(
                shared,
                agent,
                Status::Failed,
                "interrupted by restart",
                &text,
            )
            .await?;
        }
    }
    Ok(())
}

async fn last_text<C: Components>(shared: &Shared<C>, chat: ChatId) -> Result<String, Aux> {
    let events = log::decode_all(
        shared
            .db()
            .reader()
            .await?
            .run(move |c| c.turns().events_after(chat, None))
            .await?,
    )
    .map_err(Aux::from)?;
    Ok(events
        .iter()
        .rev()
        .find_map(|e| match &e.payload {
            Payload::Message(m) if m.role == Role::Assistant => Some(items::text(&m.blocks)),
            _ => None,
        })
        .unwrap_or_default())
}

pub struct WorkPort;

impl<C: Components> ports::WorkPort<C> for WorkPort {
    fn role_line<'a>(
        &'a self,
        shared: &'a Shared<C>,
        chat: ChatId,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let agent = own(shared, chat).await.ok()?;
            Some(format!(
                "[team] You are subagent \"{}\" of chat {}. Only your lead reads you. Finish with agent_done.",
                sanitize_inline(&agent.name),
                agent.parent.0
            ))
        })
    }

    fn inbox_lines<'a>(
        &'a self,
        shared: &'a Shared<C>,
        chat: ChatId,
        _budget: u32,
    ) -> BoxFuture<'a, Vec<String>> {
        Box::pin(async move {
            let Ok(agents) = children(shared, chat).await else {
                return Vec::new();
            };
            let unread: u32 = agents.iter().map(|a| a.unread).sum();
            if unread == 0 {
                return Vec::new();
            }
            let names = agents
                .iter()
                .filter(|a| a.unread > 0)
                .map(|a| sanitize_inline(&a.name))
                .collect::<Vec<_>>()
                .join(", ");
            vec![format!(
                "[team] {unread} unread letters from {names} — use agents_wait; denied/expired permission is a result to report, not to retry."
            )]
        })
    }

    fn live_subagents<'a>(&'a self, shared: &'a Shared<C>, chat: ChatId) -> BoxFuture<'a, u32> {
        Box::pin(async move {
            children(shared, chat)
                .await
                .map_or(0, |a| a.iter().filter(|a| live(a.status)).count() as u32)
        })
    }

    fn on_turn_closed<'a>(
        &'a self,
        shared: &'a Shared<C>,
        chat: ChatId,
        turn: &'a TurnId,
        outcome: TurnOutcome,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Err(error) = closed(shared, chat, turn, outcome).await {
                ::log::error!(chat:% = chat, error:% = error; "failed to settle agent");
            }
        })
    }
}

async fn closed<C: Components>(
    shared: &Shared<C>,
    chat: ChatId,
    turn: &TurnId,
    outcome: TurnOutcome,
) -> Result<(), Aux> {
    let _gate = shared.turns().slot(chat).gate.clone().lock_owned().await;
    let Some(agent) = shared
        .db()
        .reader()
        .await?
        .run(move |c| c.agents().get(chat))
        .await?
    else {
        return Ok(());
    };
    if shared
        .turns()
        .live(chat)
        .as_ref()
        .is_some_and(|current| current != turn)
    {
        return Ok(());
    }
    let id = turn.clone();
    let events = log::decode_all(
        shared
            .db()
            .reader()
            .await?
            .run(move |c| c.turns().turn_events_after(&id, EventId(0)))
            .await?,
    )
    .map_err(Aux::from)?;
    let text = events
        .iter()
        .rev()
        .find_map(|e| match &e.payload {
            Payload::Message(m) if m.role == Role::Assistant => Some(items::text(&m.blocks)),
            _ => None,
        })
        .unwrap_or_default();
    let ending = events.iter().rev().find_map(|e| match &e.payload {
        Payload::TurnEnded(e) => Some(e),
        _ => None,
    });
    if ending.is_some_and(|e| {
        matches!(
            e.by,
            Some(EndedBy::Shutdown | EndedBy::ChatDeleted | EndedBy::Gc)
        )
    }) {
        let now = shared.now();
        if shared
            .db()
            .write(move |tx| tx.agents().set_status(chat, Status::Stopped, now))
            .await?
        {
            shared.bump(Metric::AgentsStopped, "shutdown").await;
        }
        shared.access().agents_changed.notify_waiters();
        return Ok(());
    }
    match agent.status {
        Status::Done => notice(shared, &agent, Status::Done, "finished", &text).await,
        Status::Running => {
            let (status, reason) = match outcome {
                TurnOutcome::Completed => (
                    Status::Idle,
                    if ending.is_some_and(|e| matches!(e.by, Some(EndedBy::StepLimit))) {
                        "step limit".to_owned()
                    } else {
                        "silent — report: none".to_owned()
                    },
                ),
                TurnOutcome::Cancelled => (Status::Stopped, "turn cancelled".to_owned()),
                _ => (
                    Status::Failed,
                    format!(
                        "turn failed: {}",
                        ending
                            .and_then(|e| e.error.as_deref())
                            .unwrap_or("interrupted")
                    ),
                ),
            };
            settle(shared, agent, status, &reason, &text).await
        }
        _ => Ok(()),
    }
}

/// Письмо становится прочитанным в той же транзакции, что и сообщение лога.
pub async fn flush_inbox<C: Components>(
    shared: &Shared<C>,
    chat: ChatId,
    turn: &TurnId,
    segment: SegmentId,
) -> Result<(), Aux> {
    let (turn, now, budget, ratio) = (
        turn.clone(),
        shared.now(),
        shared.cfg().work.inbox_budget,
        shared.cfg().context.ratio_default,
    );
    let appended = shared
        .db()
        .write(move |tx| {
            let agents = tx.agents().list(Some(chat))?;
            let messages = tx.agents().unread(chat, true)?;
            let total = messages.len();
            let mut spent = 0usize;
            let mut result = Vec::new();
            for (index, message) in messages.into_iter().enumerate() {
                let Some(agent) = agents.iter().find(|a| a.chat == message.agent) else {
                    continue;
                };
                let mut text = letter(agent, &message.body, true);
                let cost = (text.chars().count() as f64 / ratio).ceil() as usize;
                let overflow = spent.saturating_add(cost) > budget as usize;
                if overflow {
                    text = format!("[team] {} more letters — use agents_wait", total - index);
                }
                spent += cost;
                let event = log::append(
                    tx,
                    Append {
                        chat,
                        segment,
                        turn_id: Some(&turn),
                        step: None,
                        now,
                    },
                    &Payload::Message(MessagePayload::soul(vec![Block::text(text.clone())])),
                )?;
                tx.turns().insert_message(&NewMessage {
                    chat,
                    event,
                    role: Role::User,
                    sender: Some(&PersonId::system()),
                    external_id: None,
                    reply_to_external_id: None,
                    turn_id: Some(&turn),
                })?;
                if !overflow {
                    tx.agents().read_letters(&[message.id], now)?;
                }
                result.push((event, text));
                if overflow {
                    break;
                }
            }
            DbResult::Ok((turn, result))
        })
        .await?;
    let slot = shared.turns().slot(chat);
    for (event, text) in appended.1 {
        slot.feed.note(event);
        slot.feed.derived(
            event,
            0,
            FeedEvent::ItemCompleted {
                item: feed::item(
                    feed::logged_id(event, 0),
                    Some(&appended.0),
                    event,
                    ItemStatus::Completed,
                    ItemBody::UserMessage {
                        text,
                        sender: Some(PersonId::system()),
                        master: false,
                        external_id: None,
                    },
                ),
                replaces: None,
            },
        );
    }
    Ok(())
}
