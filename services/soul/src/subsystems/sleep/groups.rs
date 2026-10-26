//! Фаза групп: ограниченный снимок, vet и dry-run перед изменением overlay.

use super::memory::{Summary, ToolResult};
use crate::{
    components::Components,
    db::{
        Db as _, DbResult, Queries as _,
        stats::{Metric, ago},
    },
    shared::Shared,
    subsystems::tools::{
        maintenance::{self, Group, GroupOp},
        registry::Catalog,
        schema::input_schema,
    },
};
use eva::{data, time::Timestamp};
use eva_protocol::types::{
    RunId,
    sleep::{PhaseOp, Verdict},
    tool::{Need, ToolDecl},
};
use serde_json::{Value, json};

#[data]
pub struct Create {
    pub path: String,
    pub about: String,
    pub why: String,
}
#[data]
pub struct Move {
    pub tool: String,
    pub to: String,
    pub why: String,
}
#[data]
pub struct Describe {
    pub path: String,
    pub about: String,
    pub why: String,
}
#[data]
pub struct Delete {
    pub path: String,
    pub why: String,
}

pub fn decls() -> Vec<ToolDecl> {
    [
        (
            "sleep.groups.create",
            "Create a tool group during sleep, with a reason.",
            input_schema::<Create>(),
        ),
        (
            "sleep.groups.move",
            "Move a shown tool or group during sleep, with a reason.",
            input_schema::<Move>(),
        ),
        (
            "sleep.groups.describe",
            "Update a shown group description during sleep, with a reason.",
            input_schema::<Describe>(),
        ),
        (
            "sleep.groups.delete",
            "Delete an empty shown tool group during sleep, with a reason.",
            input_schema::<Delete>(),
        ),
    ]
    .into_iter()
    .map(|(path, description, input)| ToolDecl {
        path: path.into(),
        description: description.into(),
        input,
        master_only: true,
        trusted_only: true,
        needs: vec![Need::Sleep],
        ends_turn: false,
        parallel_safe: false,
        waits_on_cancel: false,
        result: Default::default(),
        timeout: None,
        idempotent: false,
    })
    .collect()
}

fn parse(tool: &str, input: &Value) -> Result<(Group, String), String> {
    fn get<T: serde::de::DeserializeOwned>(input: &Value) -> Result<T, String> {
        serde_json::from_value(input.clone()).map_err(|error| format!("invalid input: {error}"))
    }
    Ok(match tool {
        "sleep_groups_create" => {
            let args: Create = get(input)?;
            (
                Group {
                    op: GroupOp::Create,
                    path: args.path,
                    to: None,
                    about: Some(args.about),
                },
                args.why,
            )
        }
        "sleep_groups_move" => {
            let args: Move = get(input)?;
            (
                Group {
                    op: GroupOp::Move,
                    path: args.tool,
                    to: Some(args.to),
                    about: None,
                },
                args.why,
            )
        }
        "sleep_groups_describe" => {
            let args: Describe = get(input)?;
            (
                Group {
                    op: GroupOp::Describe,
                    path: args.path,
                    to: None,
                    about: Some(args.about),
                },
                args.why,
            )
        }
        "sleep_groups_delete" => {
            let args: Delete = get(input)?;
            (
                Group {
                    op: GroupOp::Delete,
                    path: args.path,
                    to: None,
                    about: None,
                },
                args.why,
            )
        }
        _ => return Err("unknown sleep groups tool".into()),
    })
}

pub struct Phase<C: Components> {
    shared: Shared<C>,
    now: Timestamp,
    snapshot: Catalog,
    prompt: String,
    applied: u32,
    discarded: u32,
    ops: Vec<PhaseOp>,
    dry_run: bool,
}

impl<C: Components> Phase<C> {
    pub async fn build(shared: &Shared<C>, _run: RunId, now: Timestamp) -> DbResult<Self> {
        Self::build_with_dry_run(shared, _run, now, shared.cfg().sleep.dry_run).await
    }

    pub async fn build_with_dry_run(
        shared: &Shared<C>,
        _run: RunId,
        now: Timestamp,
        dry_run: bool,
    ) -> DbResult<Self> {
        let snapshot = (*shared.tools().get()).clone();
        let since = shared
            .day_zone()
            .day(ago(now, std::time::Duration::from_secs(30 * 86_400)));
        let usage = shared
            .db()
            .reader()
            .await?
            .run(move |conn| conn.tools().usage(&since))
            .await?;
        let mut calls = std::collections::BTreeMap::new();
        for row in usage {
            *calls.entry(row.tool).or_insert(0_u64) += u64::from(row.calls);
        }
        let mut prompt = maintenance::prompt(&snapshot);
        prompt.push_str("\nTool names and recent use:\n");
        for entry in snapshot.iter() {
            prompt.push_str(&format!(
                "{}: name {}, length {}, calls {}, collision {}\n",
                entry.path(),
                entry.canonical,
                entry.canonical.len(),
                calls.get(&entry.id).copied().unwrap_or(0),
                entry.lost_collision
            ));
        }
        Ok(Self {
            shared: shared.clone(),
            now,
            snapshot,
            prompt,
            applied: 0,
            discarded: 0,
            ops: Vec::new(),
            dry_run,
        })
    }
    pub fn prompt(&self) -> &str {
        &self.prompt
    }
    pub async fn call(&mut self, tool: &str, input: Value) -> ToolResult {
        let (args, why) = match parse(tool, &input) {
            Ok(op) => op,
            Err(reason) => return self.reject(tool, input, reason),
        };
        if why.trim().is_empty() {
            return self.reject(tool, input, "why required".into());
        }
        if self.applied >= self.shared.cfg().sleep.groups.max_ops {
            return self.reject(tool, input, "max ops".into());
        }
        if !matches!(args.op, GroupOp::Create)
            && !self.snapshot.groups().contains_key(&args.path)
            && self.snapshot.resolve(&args.path).is_empty()
        {
            return self.reject(tool, input, "id not shown".into());
        }
        if let Some(to) = &args.to
            && !self.snapshot.groups().contains_key(to)
        {
            return self.reject(tool, input, "id not shown".into());
        }
        let next = match maintenance::preview(&self.snapshot, &args, self.shared.cfg()) {
            Ok(next) => next,
            Err(reason) => return self.reject(tool, input, reason),
        };
        let dry = self.dry_run;
        if !dry {
            match maintenance::group(&self.shared, args, "sleep").await {
                Ok(result) if !result.is_error => {}
                Ok(result) => return self.reject(tool, input, format!("{:?}", result.content)),
                Err(error) => return self.reject(tool, input, format!("database error: {error}")),
            }
        }
        self.snapshot = next;
        self.applied += 1;
        self.ops.push(PhaseOp {
            tool: tool.into(),
            input,
            verdict: if dry {
                Verdict::WouldApply
            } else {
                Verdict::Applied
            },
        });
        ToolResult {
            is_error: false,
            output: json!({"applied":!dry,"would_apply":dry}),
        }
    }
    fn reject(&mut self, tool: &str, input: Value, reason: String) -> ToolResult {
        self.discarded += 1;
        self.ops.push(PhaseOp {
            tool: tool.into(),
            input,
            verdict: Verdict::Discarded {
                reason: reason.clone(),
            },
        });
        ToolResult {
            is_error: true,
            output: json!({"discarded":reason}),
        }
    }
    pub async fn finish(self) -> DbResult<Summary> {
        let (applied, discarded, now, zone) = (
            self.applied,
            self.discarded,
            self.now,
            self.shared.day_zone(),
        );
        self.shared
            .db()
            .write(move |tx| {
                tx.stats().bump_by(
                    Metric::SleepOpsApplied,
                    "groups",
                    u64::from(applied),
                    now,
                    &zone,
                )?;
                tx.stats().bump_by(
                    Metric::SleepOpsDiscarded,
                    "groups",
                    u64::from(discarded),
                    now,
                    &zone,
                )
            })
            .await?;
        Ok(Summary {
            ops_json: serde_json::to_string(&self.ops).map_err(crate::db::DbError::corrupt)?,
            applied,
            discarded,
            forgets: 0,
        })
    }
}
