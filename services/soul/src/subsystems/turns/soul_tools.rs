//! Тулы soul этой подсистемы (`01 §4.1`, `§6.5`, `§8.1`, `02 §5`, `§7`,
//! `07 §4.1`, `§5`) от идентичности `soul`; тулы чужих глав — порт
//! `Toolset`.

use eva::{data, time::HumanDuration};
use serde_json::{Value, json};

use eva_protocol::{
    errors::Aux,
    types::{
        Effort, ToolId, Tseq,
        tool::{Exposure, GroupDecl, Need, ResultForm, ToolDecl},
    },
    wire::tools::ToolResult,
};

use crate::{
    components::Components,
    config::Config,
    db::{Db as _, DbResult, Queries as _},
    subsystems::{
        tools::{
            canonical::canonical,
            catalog::{self, first_sentence},
            find,
            schema::input_schema,
            step::{Named, StepSpec, nearest},
        },
        turns::{
            dispatch::cut_tail, engine::Turn, estimate::Estimator, marker, pending, plan,
            ports::ToolsetCall,
        },
    },
};

/// Что вызов меняет в туре; применяется после батча.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Effects {
    pub effort: Option<Effort>,
    pub compact: bool,
    pub plan_changed: bool,
    pub loads: Vec<ToolId>,
}

impl Effects {
    pub fn merge(&mut self, other: &Self) {
        if other.effort.is_some() {
            self.effort = other.effort;
        }
        self.compact |= other.compact;
        self.plan_changed |= other.plan_changed;
        self.loads.extend(other.loads.iter().copied());
    }
}

/// Re-read a full tool result by its id, in line windows.
#[data]
pub struct Recall {
    /// Result id: `t17` or `17`.
    pub id: RecallId,
    /// First line to show, from 1.
    pub offset: Option<u32>,
    /// Lines to show.
    pub limit: Option<u32>,
}

#[data]
#[serde(untagged)]
pub enum RecallId {
    Number(i64),
    Text(String),
}

/// Raise or lower reasoning effort for the rest of this turn.
#[data]
pub struct SetEffort {
    /// low, medium, high or max.
    pub effort: Effort,
}

/// Compact this chat's context at the next step.
#[data]
pub struct Compact {}

/// Load reserved tools so they become callable from the next step.
#[data]
pub struct Load {
    /// Tool or group paths (`memory.history`, `mcp.github`); canonical names work too.
    pub paths: Vec<String>,
}

/// Search tools by what they do.
#[data]
pub struct Find {
    pub query: String,
    /// At most this many results, up to 20.
    pub limit: Option<u8>,
}

/// List the tool tree.
#[data]
pub struct List {
    /// Subtree path; root when omitted.
    pub path: Option<String>,
    /// Group levels to descend.
    pub depth: Option<u8>,
}

#[data]
pub struct SleepDone {
    pub report: String,
}

fn decl<I: eva::_priv::schemars::JsonSchema>(path: &str, description: &str) -> ToolDecl {
    ToolDecl {
        path: path.to_owned(),
        description: description.to_owned(),
        input: input_schema::<I>(),
        master_only: false,
        trusted_only: false,
        needs: Vec::new(),
        ends_turn: false,
        parallel_safe: false,
        waits_on_cancel: false,
        result: ResultForm::Windowed,
        timeout: None,
        idempotent: false,
    }
}

/// Декларации тулов подсистем `turns` и `tools`.
pub fn groups() -> Vec<GroupDecl> {
    [
        (
            "context",
            "Read earlier results and manage the conversation context",
        ),
        ("tools", "Find, load and organize available tools"),
        ("chats", "Find, read and manage conversations"),
        ("persons", "Manage people and their identities"),
        ("memory", "Remember and recall facts, events and procedures"),
        ("notes", "Maintain this conversation's notebook"),
        ("cron", "Schedule recurring and one-time tasks"),
        ("stats", "Usage, spend and health of this agent"),
        ("chat", "Ask the human for input"),
        ("plan", "Track the work plan"),
        ("agents", "Delegate work and coordinate subagents"),
        ("agent", "Report progress and results to the lead agent"),
        ("fs", "Read and edit files on the working endpoint"),
        ("shell", "Run commands on the working endpoint"),
        ("hands", "Manage permissions for the working endpoint"),
        ("files", "Read and save conversation attachments"),
        ("sleep", "Review memory and tool groups during maintenance"),
        ("system", "Inspect and maintain soul connections"),
        ("system.modules", "List modules and manage client tokens"),
    ]
    .into_iter()
    .map(|(path, about)| GroupDecl {
        path: path.into(),
        about: about.into(),
    })
    .collect()
}

/// Декларации тулов подсистем `turns` и `tools`.
pub fn decls(cfg: &Config) -> Vec<ToolDecl> {
    let ask_timeout = cfg.work.ask_ttl.0 + std::time::Duration::from_secs(10);
    let mut tools = vec![
        ToolDecl {
            result: ResultForm::Uncapped,
            parallel_safe: true,
            ..decl::<Recall>(
                "context.recall",
                "Re-read the full output of an earlier tool result by its id (t17), in line windows.",
            )
        },
        ToolDecl {
            master_only: true,
            ..decl::<SetEffort>(
                "context.effort",
                "Set reasoning effort for the rest of this turn.",
            )
        },
        ToolDecl {
            master_only: true,
            ..decl::<Compact>(
                "context.compact",
                "Compact this chat's context at the next step.",
            )
        },
        ToolDecl {
            needs: vec![Need::AskUi, Need::Lead],
            timeout: Some(HumanDuration(ask_timeout)),
            ..decl::<pending::Ask>(
                "chat.ask",
                "Ask the human 1-3 questions with 2-8 button options each and wait for the answer.",
            )
        },
        ToolDecl {
            master_only: true,
            needs: vec![Need::Work],
            ..decl::<plan::Update>(
                "plan.update",
                "Update the turn plan: init, start, done, drop, append or view tasks.",
            )
        },
        decl::<Load>(
            "tools.load",
            "Load reserved tools by path so they become callable from the next step.",
        ),
        ToolDecl {
            parallel_safe: true,
            ..decl::<Find>(
                "tools.find",
                "Find tools by what they do; returns names, paths and whether they are loaded.",
            )
        },
        ToolDecl {
            parallel_safe: true,
            ..decl::<List>("tools.list", "List the tool tree under a path.")
        },
    ];
    tools.push(ToolDecl {
        master_only: true,
        needs: vec![Need::Sleep],
        ends_turn: true,
        ..decl::<SleepDone>(
            "sleep.done",
            "Finish this sleep phase and report the changes.",
        )
    });
    for name in crate::subsystems::sleep::memory::TOOLS {
        tools.push(ToolDecl {
            path: name.replacen("sleep_memory_", "sleep.memory.", 1),
            description: String::from("Apply a reviewed memory maintenance operation with a reason."),
            input: serde_json::json!({"type":"object","properties":{"id":{"type":"string"},"ids":{"type":"array","items":{"type":"string"}},"text":{"type":"string"},"kind":{"type":"string"},"importance":{"type":"integer"},"into":{"type":"array","items":{"type":"object"}},"why":{"type":"string"}},"required":["why"]}),
            master_only: true, trusted_only: true, needs: vec![Need::Sleep], ends_turn: false, parallel_safe: false, waits_on_cancel: false, result: ResultForm::Windowed, timeout: None, idempotent: false,
        });
    }
    tools.extend(crate::subsystems::sleep::groups::decls());
    tools
}

fn ok_json(value: &Value) -> ToolResult {
    ToolResult::ok(vec![eva_protocol::types::tool::ToolContent::text(
        value.to_string(),
    )])
}

fn ok_text(text: impl Into<String>) -> ToolResult {
    ToolResult::ok(vec![eva_protocol::types::tool::ToolContent::text(text)])
}

fn parse<I: serde::de::DeserializeOwned>(input: Value) -> Result<I, ToolResult> {
    serde_json::from_value(input).map_err(|e| ToolResult::error(format!("invalid input: {e}")))
}

/// Исполнение тула soul; `Err` — беда soul (`Fatal`).
pub async fn call<C: Components>(
    turn: &Turn<C>,
    spec: &StepSpec,
    call_id: &str,
    input: Value,
) -> Result<(ToolResult, Effects), Aux> {
    let mut effects = Effects::default();
    let result = match spec.route.as_str() {
        "sleep_done" => match parse::<SleepDone>(input) {
            Ok(args) => {
                *turn.sleep_report.lock().await = Some(args.report);
                ok_text("sleep phase completed")
            }
            Err(e) => e,
        },
        name if name.starts_with("sleep_memory_") => match &turn.sleep {
            Some(phase) => {
                let result = phase.lock().await.call(name, input).await;
                ToolResult {
                    content: vec![eva_protocol::types::tool::ToolContent::text(
                        result.output.to_string(),
                    )],
                    is_error: result.is_error,
                }
            }
            None => ToolResult::error("memory phase unavailable"),
        },
        name if name.starts_with("sleep_groups_") => match &turn.group_phase {
            Some(phase) => {
                let result = phase.lock().await.call(name, input).await;
                ToolResult {
                    content: vec![eva_protocol::types::tool::ToolContent::text(
                        result.output.to_string(),
                    )],
                    is_error: result.is_error,
                }
            }
            None => ToolResult::error("groups phase unavailable"),
        },
        "context_recall" => match parse::<Recall>(input) {
            Ok(args) => recall(turn, args).await?,
            Err(e) => e,
        },
        "context_effort" => match parse::<SetEffort>(input) {
            Ok(args) => {
                effects.effort = Some(args.effort);
                ok_text(marker::effort_set(args.effort))
            }
            Err(e) => e,
        },
        "context_compact" => {
            effects.compact = true;
            ok_text(marker::COMPACTION_SCHEDULED)
        }
        "chat_ask" => match parse::<pending::Ask>(input) {
            Ok(args) => pending::ask(turn, call_id, args).await?,
            Err(e) => e,
        },
        "plan_update" => match parse::<plan::Update>(input) {
            Ok(args) => {
                let (result, changed) = plan::update(turn, args).await?;
                effects.plan_changed = changed;
                result
            }
            Err(e) => e,
        },
        "tools_load" => match parse::<Load>(input) {
            Ok(args) => load(turn, args, &mut effects).await?,
            Err(e) => e,
        },
        "tools_find" => match parse::<Find>(input) {
            Ok(args) => find(turn, args),
            Err(e) => e,
        },
        "tools_list" => match parse::<List>(input) {
            Ok(args) => list(turn, args),
            Err(e) => e,
        },
        _ => {
            let shared = &turn.shared;
            let ctx = turn.ctx.wire(&turn.live.turn);
            let view = turn.ctx.view();
            let loaded = turn.load().await?;
            let model = turn.model_id();
            let ratio = shared
                .db()
                .reader()
                .await?
                .run(move |c| c.turns().ratio(&model))
                .await?;
            shared
                .ports()
                .toolset
                .call(
                    shared,
                    ToolsetCall {
                        canonical: &spec.route,
                        call_id,
                        input,
                        ctx: &ctx,
                        view: &view,
                        hot_ids: &loaded.snapshot.hot_memory_ids,
                        ratio,
                    },
                )
                .await?
        }
    };
    Ok((result, effects))
}

/// `context_recall` (`01 §4.1`): окно полного `content` с честной шапкой.
async fn recall<C: Components>(turn: &Turn<C>, args: Recall) -> Result<ToolResult, Aux> {
    let seq = match &args.id {
        RecallId::Number(n) => Some(*n),
        RecallId::Text(t) => t
            .trim()
            .strip_prefix('t')
            .unwrap_or(t.trim())
            .parse::<i64>()
            .ok(),
    };
    let Some(seq) = seq else {
        return Ok(ToolResult::error(marker::UNKNOWN_RESULT));
    };
    let chat = turn.ctx.chat;
    let model = turn.model_id();
    let (content, ratio) = turn
        .shared
        .db()
        .reader()
        .await?
        .run(move |c| {
            DbResult::Ok((
                c.turns().result_content(chat, Tseq(seq))?,
                c.turns().ratio(&model)?,
            ))
        })
        .await?;
    let Some(content) = content else {
        return Ok(ToolResult::error(marker::UNKNOWN_RESULT));
    };
    let text = String::from_utf8_lossy(&content);
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len() as u64;
    let cfg = turn.shared.cfg();
    let from = u64::from(args.offset.unwrap_or(1).max(1));
    let limit = u64::from(args.limit.unwrap_or(cfg.work.read_default_lines).max(1));
    if from > total.max(1) {
        return Ok(ToolResult::error(format!(
            "offset {from} is past the end: t{seq} has {total} lines"
        )));
    }
    let est = Estimator::new(ratio, &cfg.context);
    let budget = cfg.work.reader_result_tokens;
    let start = usize::try_from(from - 1).unwrap_or(usize::MAX);
    let mut to = (from + limit - 1).min(total);
    loop {
        let end = usize::try_from(to).unwrap_or(usize::MAX).min(lines.len());
        let body = lines[start.min(end)..end].join("\n");
        let header = marker::lines_header(from, to, total);
        let shown = format!("{header}\n{body}");
        if est.text(shown.len()) <= budget || to <= from {
            let shown = if est.text(shown.len()) > budget {
                format!("{header}\n{}", cut_tail(&body, budget, &est))
            } else {
                shown
            };
            return Ok(ok_text(shown));
        }
        to -= 1;
    }
}

/// `tools_load` (`02 §5.2`): Reserved → `loaded` чата, Visible — уже видно,
/// Forbidden — как неизвестный (I3).
async fn load<C: Components>(
    turn: &Turn<C>,
    args: Load,
    effects: &mut Effects,
) -> Result<ToolResult, Aux> {
    let max = usize::from(turn.shared.cfg().tools.load.max_paths.max(1));
    if args.paths.is_empty() || args.paths.len() > max {
        return Ok(ToolResult::error(format!(
            "invalid input: paths takes 1..={max} entries"
        )));
    }
    let catalog = turn.shared.tools().get();
    let names = &turn.tools.names;
    let (mut loaded, mut visible, mut unknown) = (Vec::new(), Vec::new(), Vec::new());
    let mut reserve: Vec<ToolId> = Vec::new();
    for path in &args.paths {
        let found: Vec<_> = catalog
            .resolve(path)
            .into_iter()
            .filter(|e| names.contains_key(&e.canonical))
            .collect();
        if found.is_empty() {
            unknown.push(json!({
                "name": path,
                "nearest": nearest(names.keys().map(String::as_str), &canonical(path)),
            }));
        }
        for entry in found {
            match names.get(&entry.canonical) {
                Some(Named::Reserved(id)) if !reserve.contains(id) => {
                    reserve.push(*id);
                    loaded.push(entry.canonical.clone());
                }
                Some(Named::Visible(_)) if !visible.contains(&entry.canonical) => {
                    visible.push(entry.canonical.clone())
                }
                _ => {}
            }
        }
    }
    let (chat, now) = (turn.ctx.chat, turn.shared.now());
    let ids = reserve.clone();
    turn.shared
        .db()
        .write(move |tx| {
            for id in &ids {
                tx.tools().load(chat, *id, now)?;
            }
            DbResult::Ok(())
        })
        .await?;
    effects.loads = reserve;
    Ok(ok_json(&json!({
        "loaded": loaded,
        "already_visible": visible,
        "unknown": unknown,
    })))
}

fn exposure_of(names: &std::collections::BTreeMap<String, Named>, canonical: &str) -> Exposure {
    match names.get(canonical) {
        Some(Named::Visible(_)) => Exposure::Visible,
        Some(Named::Reserved(_)) => Exposure::Reserved,
        None => Exposure::Forbidden(eva_protocol::types::tool::Forbid::NotInWhitelist),
    }
}

/// `tools_find` (`02 §5.3`): корпус — Visible ∪ Reserved реестра.
fn find<C: Components>(turn: &Turn<C>, args: Find) -> ToolResult {
    let catalog = turn.shared.tools().get();
    let names = &turn.tools.names;
    let docs: Vec<(&crate::subsystems::tools::registry::Entry, String)> = catalog
        .iter()
        .filter(|e| names.contains_key(&e.canonical))
        .map(|e| (e, find::document(e, catalog.about(find::group(e)))))
        .collect();
    let limit = usize::from(
        args.limit
            .unwrap_or(turn.shared.cfg().tools.find.top_n)
            .clamp(1, 20),
    );
    let hits = find::rank(&docs, &args.query, limit);
    if hits.is_empty() {
        return ok_text("no matching tools");
    }
    let rows: Vec<Value> = hits
        .into_iter()
        .map(|e| {
            let exposure = match exposure_of(names, &e.canonical) {
                Exposure::Visible => "visible",
                _ => "reserved",
            };
            json!({
                "name": e.canonical,
                "path": e.path(),
                "about": first_sentence(&e.decl.description),
                "exposure": exposure,
            })
        })
        .collect();
    ok_json(&Value::Array(rows))
}

/// `tools_list` (`02 §7`): дерево по текущему ctx, Forbidden скрыты.
fn list<C: Components>(turn: &Turn<C>, args: List) -> ToolResult {
    let catalog = turn.shared.tools().get();
    let names = &turn.tools.names;
    let exposure = |e: &crate::subsystems::tools::registry::Entry| exposure_of(names, &e.canonical);
    let Some(listed) = catalog::subtree(
        &catalog,
        args.path.as_deref(),
        args.depth.unwrap_or(1),
        &exposure,
    ) else {
        return ToolResult::error(format!(
            "tool path {} does not exist",
            args.path.unwrap_or_default()
        ));
    };
    let tree = catalog::tree(&catalog, &listed);
    match serde_json::to_value(&tree) {
        Ok(value) => ok_json(&value),
        Err(e) => ToolResult::error(e.to_string()),
    }
}
