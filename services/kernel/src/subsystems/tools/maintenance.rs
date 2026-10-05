//! Изменения overlay: проверка всего результата перед первой записью.

use std::collections::{BTreeMap, BTreeSet};

use eva::{data, time::Timestamp};
use eva_protocol::{
    errors::Aux,
    types::{
        ToolId,
        tool::{ToolContent, ToolDecl},
    },
    wire::tools::ToolResult,
};
use serde_json::Value;

use super::{
    canonical::{canonical, under, valid_path},
    registry::{self, Catalog},
    schema::input_schema,
};
use crate::{
    components::Components,
    config::Config,
    db::{Db as _, DbResult, Queries as _, tools::Attention},
    shared::Shared,
};

#[data]
pub struct Group {
    pub op: GroupOp,
    pub path: String,
    pub to: Option<String>,
    pub about: Option<String>,
}

#[data(copy)]
pub enum GroupOp {
    Create,
    Move,
    Describe,
    Delete,
}

#[data]
pub struct Override {
    pub path: String,
    pub master_only: Option<bool>,
    pub trusted_only: Option<bool>,
    pub gate: Option<Gate>,
    pub attention: Option<AttentionChange>,
    pub note: String,
}

#[data(copy)]
pub enum Gate {
    Forbid,
    Clear,
}
#[data(copy)]
pub enum AttentionChange {
    Always,
    Reserved,
    Clear,
}

pub fn decls() -> Vec<ToolDecl> {
    [
        (
            "tools.group",
            "Create, move, describe or delete a tool group.",
            input_schema::<Group>(),
        ),
        (
            "tools.override",
            "Change tool access gates and attention policy.",
            input_schema::<Override>(),
        ),
    ]
    .into_iter()
    .map(|(path, description, input)| ToolDecl {
        path: path.into(),
        description: description.into(),
        input,
        master_only: true,
        trusted_only: true,
        needs: Vec::new(),
        ends_turn: false,
        parallel_safe: false,
        waits_on_cancel: false,
        result: Default::default(),
        timeout: None,
        idempotent: false,
    })
    .collect()
}

pub async fn call<C: Components>(
    shared: &Shared<C>,
    name: &str,
    input: Value,
) -> Result<ToolResult, Aux> {
    match name {
        "tools_group" => match serde_json::from_value(input) {
            Ok(args) => group(shared, args, "master").await,
            Err(e) => Ok(ToolResult::error(format!("invalid input: {e}"))),
        },
        "tools_override" => match serde_json::from_value::<Override>(input) {
            Err(e) => Ok(ToolResult::error(format!("invalid input: {e}"))),
            Ok(args) => {
                let entries = shared.tools().get();
                let resolved = entries.resolve(&args.path);
                let Some(entry) = resolved.first().filter(|_| resolved.len() == 1) else {
                    return Ok(ToolResult::error("path must identify exactly one tool"));
                };
                let id = entry.id;
                let now = shared.now();
                let value = shared
                    .db()
                    .write(move |tx| {
                        let mut value = tx
                            .tools()
                            .overrides()?
                            .get(&id)
                            .copied()
                            .unwrap_or_default();
                        if args.master_only.is_some() {
                            value.master_only = args.master_only;
                        }
                        if args.trusted_only.is_some() {
                            value.trusted_only = args.trusted_only;
                        }
                        if let Some(gate) = args.gate {
                            value.forbid = matches!(gate, Gate::Forbid);
                        }
                        if let Some(attention) = args.attention {
                            value.attention = match attention {
                                AttentionChange::Always => Some(Attention::Always),
                                AttentionChange::Reserved => Some(Attention::Reserved),
                                AttentionChange::Clear => None,
                            };
                        }
                        tx.tools().set_override(id, value, &args.note, now)?;
                        DbResult::Ok(value)
                    })
                    .await?;
                Ok(ToolResult::ok(vec![ToolContent::text(
                    serde_json::to_string(&value).map_err(|e| Aux::Internal(e.to_string()))?,
                )]))
            }
        },
        _ => Err(Aux::Internal(format!("unknown maintenance tool {name}"))),
    }
}

struct Change {
    groups: BTreeMap<String, String>,
    placements: BTreeMap<ToolId, String>,
    deleted: BTreeSet<String>,
}

fn vet(catalog: &Catalog, args: &Group, cfg: &Config) -> Result<Change, String> {
    let max = usize::from(cfg.tools.max_depth);
    let valid_group = |path: &str| valid_path(path, 1, max.saturating_sub(1));
    let exists = |path: &str| catalog.groups().contains_key(path);
    let about = || {
        args.about
            .as_deref()
            .filter(|about| !about.trim().is_empty() && crate::markers::check_inline(about))
            .ok_or_else(|| "about must be a non-empty line without kernel markers".to_owned())
    };
    let mut change = Change {
        groups: BTreeMap::new(),
        placements: BTreeMap::new(),
        deleted: BTreeSet::new(),
    };
    match args.op {
        GroupOp::Create => {
            if !valid_group(&args.path) {
                return Err("invalid group path".into());
            }
            if exists(&args.path) {
                return Err("group already exists".into());
            }
            change.groups.insert(args.path.clone(), about()?.to_owned());
        }
        GroupOp::Describe => {
            if !exists(&args.path) {
                return Err("unknown group".into());
            }
            change.groups.insert(args.path.clone(), about()?.to_owned());
        }
        GroupOp::Delete => {
            if !exists(&args.path) {
                return Err("unknown group".into());
            }
            if catalog.iter().any(|e| under(e.path(), &args.path))
                || catalog
                    .groups()
                    .keys()
                    .any(|g| g != &args.path && under(g, &args.path))
            {
                return Err("group is not empty".into());
            }
            change.deleted.insert(args.path.clone());
        }
        GroupOp::Move => {
            let to = args
                .to
                .as_deref()
                .ok_or_else(|| "move requires to".to_owned())?;
            if !exists(to) || !valid_group(to) {
                return Err("target group does not exist".into());
            }
            let entries = catalog.resolve(&args.path);
            if entries.is_empty() {
                return Err("unknown tool or group".into());
            }
            let is_group = exists(&args.path);
            if is_group && under(to, &args.path) {
                return Err("a group cannot be moved inside itself".into());
            }
            let kernel_roots: BTreeSet<_> = catalog
                .iter()
                .filter(|e| e.is_builtin())
                .filter_map(|e| e.path().split('.').next())
                .collect();
            for entry in &entries {
                if entry.is_builtin() && cfg.tools.core.iter().any(|p| under(entry.path(), p)) {
                    return Err("core kernel tools cannot move".into());
                }
                if !entry.is_builtin() && kernel_roots.contains(to.split('.').next().unwrap_or(to))
                {
                    return Err("module tools cannot move into kernel groups".into());
                }
                let group = if is_group {
                    let old_group = entry.path().rsplit_once('.').map_or("", |(group, _)| group);
                    format!("{to}{}", old_group.strip_prefix(&args.path).unwrap_or(""))
                } else {
                    to.to_owned()
                };
                let leaf = entry.path().rsplit('.').next().unwrap_or(entry.path());
                if !valid_path(&format!("{group}.{leaf}"), 2, max) {
                    return Err("move exceeds tools.max_depth".into());
                }
                change
                    .groups
                    .entry(group.clone())
                    .or_insert_with(|| catalog.about(&group).to_owned());
                change.placements.insert(entry.id, group);
            }
            if is_group {
                for (path, about) in catalog
                    .groups()
                    .iter()
                    .filter(|(path, _)| under(path, &args.path))
                {
                    let new = format!("{to}{}", path.strip_prefix(&args.path).unwrap_or(""));
                    change.groups.insert(new, about.clone());
                    change.deleted.insert(path.clone());
                }
            }
            let mut names = BTreeSet::new();
            for entry in catalog.iter() {
                let path = change
                    .placements
                    .get(&entry.id)
                    .map(|group| {
                        format!(
                            "{group}.{}",
                            entry.path().rsplit('.').next().unwrap_or(entry.path())
                        )
                    })
                    .unwrap_or_else(|| entry.path().to_owned());
                if !names.insert(canonical(&path)) {
                    return Err("move would create a canonical name collision".into());
                }
            }
        }
    }
    Ok(change)
}

pub(crate) fn preview(catalog: &Catalog, args: &Group, cfg: &Config) -> Result<Catalog, String> {
    let change = vet(catalog, args, cfg)?;
    let mut groups = catalog.groups().clone();
    groups.extend(change.groups);
    for path in change.deleted {
        groups.remove(&path);
    }
    let rows = catalog
        .iter()
        .map(|entry| crate::db::tools::ToolRow {
            id: entry.id,
            module: entry.module.clone(),
            path: entry.path().to_owned(),
            canonical: entry.canonical.clone(),
            decl: entry.decl.clone(),
            online: entry.online,
        })
        .collect();
    Ok(Catalog::with_placements(rows, groups, change.placements))
}

pub async fn group<C: Components>(
    shared: &Shared<C>,
    args: Group,
    source: &'static str,
) -> Result<ToolResult, Aux> {
    let now: Timestamp = shared.now();
    let cfg = shared.cfg().clone();
    let applied = shared
        .db()
        .write(move |tx| {
            let catalog = Catalog::with_placements(
                tx.tools().live()?,
                tx.tools().groups()?,
                tx.tools().placements()?,
            );
            let change = match vet(&catalog, &args, &cfg) {
                Ok(change) => change,
                Err(reason) => return Ok(Err(reason)),
            };
            for (path, about) in change.groups {
                tx.tools().put_group(&path, &about, source, now)?;
            }
            for (id, path) in change.placements {
                tx.tools().place(id, &path, source, now)?;
            }
            for path in change.deleted {
                tx.tools().delete_group(&path)?;
            }
            tx.meta()
                .increment(crate::db::meta::TOOLS_OVERLAY_VERSION)?;
            DbResult::Ok(Ok(()))
        })
        .await?;
    match applied {
        Ok(()) => {
            registry::reload(shared).await?;
            Ok(ToolResult::ok(vec![ToolContent::text("applied")]))
        }
        Err(reason) => Ok(ToolResult::error(reason)),
    }
}

pub fn prompt(catalog: &Catalog) -> String {
    let mut out =
        String::from("Review tool groups. Improve descriptions and grouping only when useful.\n");
    for (path, about) in catalog.groups() {
        out.push_str(&format!("{path}: {about}\n"));
    }
    for entry in catalog.iter().filter(|e| e.online && !e.lost_collision) {
        out.push_str(&format!("{}: {}\n", entry.path(), entry.decl.description));
    }
    out
}
