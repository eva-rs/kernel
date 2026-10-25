//! Инструменты крона: чтение через библиотеку, мутации в одной транзакции.

use std::collections::BTreeSet;

use eva::{
    data,
    time::{HumanDuration, Timestamp},
};
use eva_core::{CallStep as _, service::Cron::Trait as _};
use eva_protocol::{
    errors::{Aux, Generic},
    requests::{RunsLimit, cron::list},
    types::{
        CronJobId, Effort, ModelId, SurfaceId, ToolId,
        chat::Kind,
        tool::{ResultForm, ToolContent, ToolDecl},
    },
    wire::{
        events::{ChatEvent, Event},
        tools::ToolResult,
    },
};
use serde_json::{Value, json};

use crate::{
    components::Components,
    db::{
        Db as _, Queries as _,
        chats::NewChat,
        cron::{FirePending, JobRow},
    },
    shared::Shared,
    state::Policy,
    subsystems::{
        tools::{schema::input_schema, step::levenshtein},
        turns::ports::ToolsetCall,
    },
};

#[data]
#[serde(deny_unknown_fields)]
pub struct Create {
    pub name: String,
    pub task: String,
    pub interval: Option<HumanDuration>,
    pub at: Option<Timestamp>,
    pub expires_at: Option<Timestamp>,
    pub model: Option<ModelId>,
    pub effort: Option<Effort>,
    /// Tool paths, groups or canonical names; omitted means unrestricted.
    pub tools: Option<Vec<String>>,
}

fn change<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    <Option<T> as serde::Deserialize>::deserialize(deserializer).map(Some)
}

#[data]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub id: CronJobId,
    pub name: Option<String>,
    pub task: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "change"
    )]
    pub interval: Option<Option<HumanDuration>>,
    /// Accepted for symmetry with create; updates always preserve next_fire_at.
    pub at: Option<Timestamp>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "change"
    )]
    pub expires_at: Option<Option<Timestamp>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "change"
    )]
    pub model: Option<Option<ModelId>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "change"
    )]
    pub effort: Option<Option<Effort>>,
    /// Null removes the whitelist; an empty array allows no tools.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "change"
    )]
    pub tools: Option<Option<Vec<String>>>,
}

#[data]
#[serde(deny_unknown_fields)]
pub struct Delete {
    pub id: CronJobId,
}

#[data]
#[serde(deny_unknown_fields)]
pub struct Fire {
    pub id: CronJobId,
    pub shift: bool,
}

#[data]
#[serde(deny_unknown_fields)]
pub struct Log {
    pub job_id: Option<CronJobId>,
    #[serde(default)]
    pub limit: RunsLimit,
}

pub fn decls() -> Vec<ToolDecl> {
    [
        ("cron.create", "Create a scheduled task with a persistent chat. Supply interval or at.", input_schema::<Create>()),
        ("cron.update", "Update supplied task fields without moving the next scheduled run. Null clears optional fields.", input_schema::<Update>()),
        ("cron.delete", "Delete a scheduled task and archive its chat.", input_schema::<Delete>()),
        ("cron.fire", "Persist a request to run a task at the next scheduler tick. Shift sets the following slot from that run.", input_schema::<Fire>()),
        ("cron.list", "List scheduled tasks and their current tool whitelists.", input_schema::<list::Args>()),
        ("cron.log", "Read recent scheduled task runs, newest first.", input_schema::<Log>()),
    ].into_iter().map(|(path, description, input)| {
        let read = matches!(path, "cron.list" | "cron.log");
        ToolDecl { path: path.into(), description: description.into(), input, master_only: true,
            trusted_only: true, needs: vec![], ends_turn: false, parallel_safe: read,
            waits_on_cancel: false, result: ResultForm::Windowed, timeout: None, idempotent: read }
    }).collect()
}

fn api_error<E: std::fmt::Display>(error: Generic<E>) -> Aux {
    match error {
        Generic::Aux(error) => error,
        Generic::Spec(error) => Aux::Invalid(error.to_string()),
    }
}

fn parse<T: serde::de::DeserializeOwned>(input: Value) -> Result<T, Aux> {
    serde_json::from_value(input).map_err(|e| Aux::Invalid(format!("invalid input: {e}")))
}

fn json_result(value: impl serde::Serialize) -> Result<ToolResult, Aux> {
    Ok(ToolResult::ok(vec![ToolContent::text(
        serde_json::to_string(&value).map_err(|e| Aux::Internal(e.to_string()))?,
    )]))
}

fn resolve<C: Components>(shared: &Shared<C>, paths: Vec<String>) -> Result<Vec<ToolId>, Aux> {
    let catalog = shared.tools().get();
    let mut ids = BTreeSet::new();
    for path in paths {
        let entries = catalog.resolve(&path);
        if entries.is_empty() && !catalog.groups().contains_key(&path) {
            let mut nearest: Vec<_> = catalog
                .iter()
                .map(|entry| {
                    (
                        levenshtein(&entry.canonical, &path),
                        entry.canonical.as_str(),
                    )
                })
                .collect();
            nearest.sort_unstable();
            let hints: Vec<_> = nearest.into_iter().take(3).map(|(_, name)| name).collect();
            return Err(Aux::Invalid(format!(
                "unknown tool or group {path:?}; nearest: {}",
                hints.join(", ")
            )));
        }
        ids.extend(entries.into_iter().map(|e| e.id));
    }
    Ok(ids.into_iter().collect())
}

fn valid<C: Components>(
    shared: &Shared<C>,
    name: &str,
    task: &str,
    interval: Option<HumanDuration>,
    model: Option<&ModelId>,
) -> Result<(), Aux> {
    if name.trim().is_empty() || task.trim().is_empty() {
        return Err(Aux::Invalid("name and task must be non-empty".into()));
    }
    if interval.is_some_and(|i| i.0.is_zero() || i.0.as_nanos() > i64::MAX as u128) {
        return Err(Aux::Invalid(
            "interval must be positive and fit signed nanoseconds".into(),
        ));
    }
    if let Some(model) = model
        && shared.models().get(model).is_none()
    {
        return Err(Aux::Invalid(format!("unknown model {model}")));
    }
    Ok(())
}

fn timestamp(at: Option<Timestamp>) -> Result<(), Aux> {
    if at.is_some_and(|t| t.as_nanos().get() > i64::MAX as u64) {
        return Err(Aux::Invalid("timestamp must fit signed nanoseconds".into()));
    }
    Ok(())
}

pub async fn call<C: Components>(
    shared: &Shared<C>,
    call: ToolsetCall<'_>,
) -> Result<ToolResult, Aux> {
    let result = async {
        Policy::cron_tool(call.view)?;
        match call.canonical {
            "cron_list" => {
                let args: list::Args = parse(call.input)?;
                let rows = crate::session::internal(shared)
                    .cron()
                    .inner_mut()
                    .list()
                    .call(args)
                    .await
                    .map_err(api_error)?;
                json_result(rows.jobs)
            }
            "cron_log" => {
                let args: Log = parse(call.input)?;
                let rows = crate::session::internal(shared)
                    .cron()
                    .runs(args.job_id, args.limit, None)
                    .await
                    .map_err(api_error)?;
                json_result(rows.found)
            }
            "cron_create" => create(shared, parse(call.input)?).await,
            "cron_update" => update(shared, parse(call.input)?).await,
            "cron_delete" => {
                let args: Delete = parse(call.input)?;
                let now = shared.now();
                let row = shared
                    .db()
                    .write(move |tx| -> Result<_, Aux> {
                        let chat = tx
                            .cron()
                            .delete_job(&args.id)?
                            .ok_or_else(|| Aux::NotFound(format!("cron job {}", args.id)))?;
                        tx.chats().set_deleted(chat, Some(now))?;
                        tx.chats()
                            .get(chat)?
                            .ok_or_else(|| Aux::Internal("cron chat vanished".into()))
                    })
                    .await?;
                shared.bus().publish(
                    Event::ChatDeleted(ChatEvent {
                        chat: row.id,
                        surface: row.surface.id.clone(),
                        external_id: row.alias.map(|alias| alias.external_id),
                    }),
                    Some(row.surface),
                );
                json_result(json!({"deleted": true}))
            }
            "cron_fire" => {
                let args: Fire = parse(call.input)?;
                let id = args.id.clone();
                shared
                    .db()
                    .write(move |tx| -> Result<(), Aux> {
                        let pending = if args.shift {
                            FirePending::Shift
                        } else {
                            FirePending::Keep
                        };
                        if !tx.cron().request_fire(&args.id, pending)? {
                            return Err(Aux::NotFound(format!("cron job {}", args.id)));
                        }
                        Ok(())
                    })
                    .await?;
                json_result(json!({"id": id}))
            }
            _ => Err(Aux::Internal(format!(
                "unknown cron tool {}",
                call.canonical
            ))),
        }
    }
    .await;
    match result {
        Err(error @ (Aux::Db(_) | Aux::Internal(_))) => Err(error),
        Err(error) => Ok(ToolResult::error(error.to_string())),
        Ok(result) => Ok(result),
    }
}

async fn create<C: Components>(shared: &Shared<C>, args: Create) -> Result<ToolResult, Aux> {
    valid(
        shared,
        &args.name,
        &args.task,
        args.interval,
        args.model.as_ref(),
    )?;
    timestamp(args.at)?;
    timestamp(args.expires_at)?;
    let now = shared.now();
    let next = match (args.at, args.interval) {
        (Some(at), _) => at,
        (None, Some(interval)) => {
            let nanos = now
                .as_nanos()
                .get()
                .checked_add(interval.0.as_nanos() as u64)
                .filter(|n| *n <= i64::MAX as u64)
                .ok_or_else(|| Aux::Invalid("next run exceeds timestamp range".into()))?;
            Timestamp::from_nanos(
                std::num::NonZeroU64::new(nanos)
                    .ok_or_else(|| Aux::Invalid("next run is zero".into()))?,
            )
        }
        (None, None) => return Err(Aux::Invalid("interval or at is required".into())),
    };
    let tools = args.tools.map(|paths| resolve(shared, paths)).transpose()?;
    let id: CronJobId = shared.make_world().mint();
    let (row, surface) = shared
        .db()
        .write(move |tx| -> Result<_, Aux> {
            if tx.cron().name_taken(&args.name, &id)? {
                return Err(Aux::Invalid(format!(
                    "cron name {:?} already exists",
                    args.name
                )));
            }
            let folder = tx
                .folders()
                .by_name(&SurfaceId::from("system"), "cron")?
                .ok_or_else(|| Aux::Internal("cron folder is missing".into()))?;
            let title = format!("cron: {}", crate::markers::sanitize_inline(&args.name));
            let chat = tx.chats().insert(&NewChat {
                folder_id: folder.id,
                kind: Kind::Persistent,
                title: Some(title.clone()),
                project: None,
                workdir: None,
                workdir_on: None,
                now,
            })?;
            tx.chats().index_title(chat, Some(&title))?;
            let row = JobRow {
                id,
                name: args.name,
                task: args.task,
                interval: args.interval,
                next_fire_at: Some(next),
                expires_at: args.expires_at,
                model: args.model,
                effort: args.effort,
                tools: None,
                chat,
                runs_total: 0,
                spent_total: 0.0,
                created_at: now,
            };
            tx.cron().insert_job(&row, tools.as_deref())?;
            Ok((row, folder.surface))
        })
        .await?;
    shared.bus().publish(
        Event::ChatCreated(ChatEvent {
            chat: row.chat,
            surface: surface.id.clone(),
            external_id: None,
        }),
        Some(surface),
    );
    json_result(json!({"id": row.id, "next_fire_at": row.next_fire_at, "chat_id": row.chat}))
}

async fn update<C: Components>(shared: &Shared<C>, args: Update) -> Result<ToolResult, Aux> {
    if let Some(Some(ref model)) = args.model
        && shared.models().get(model).is_none()
    {
        return Err(Aux::Invalid(format!("unknown model {model}")));
    }
    timestamp(args.at)?;
    timestamp(args.expires_at.flatten())?;
    let tools = args
        .tools
        .map(|paths| paths.map(|paths| resolve(shared, paths)).transpose())
        .transpose()?;
    if args.name.as_ref().is_some_and(|n| n.trim().is_empty())
        || args.task.as_ref().is_some_and(|t| t.trim().is_empty())
    {
        return Err(Aux::Invalid("name and task must be non-empty".into()));
    }
    if args
        .interval
        .flatten()
        .is_some_and(|i| i.0.is_zero() || i.0.as_nanos() > i64::MAX as u128)
    {
        return Err(Aux::Invalid(
            "interval must be positive and fit signed nanoseconds".into(),
        ));
    }
    let row = shared
        .db()
        .write(move |tx| -> Result<JobRow, Aux> {
            let mut row = tx
                .cron()
                .job(&args.id)?
                .ok_or_else(|| Aux::NotFound(format!("cron job {}", args.id)))?;
            if let Some(name) = args.name {
                row.name = name;
            }
            if let Some(task) = args.task {
                row.task = task;
            }
            if let Some(interval) = args.interval {
                row.interval = interval;
            }
            if let Some(expires) = args.expires_at {
                row.expires_at = expires;
            }
            if let Some(model) = args.model {
                row.model = model;
            }
            if let Some(effort) = args.effort {
                row.effort = effort;
            }
            if tx.cron().name_taken(&row.name, &row.id)? {
                return Err(Aux::Invalid(format!(
                    "cron name {:?} already exists",
                    row.name
                )));
            }
            tx.cron()
                .update_job(&row, tools.as_ref().map(|ids| ids.as_deref()))?;
            Ok(row)
        })
        .await?;
    json_result(json!({"id": row.id, "next_fire_at": row.next_fire_at}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_schema_and_parser_preserve_missing_null_and_empty_whitelist() {
        let schema = input_schema::<Update>();
        assert!(schema.get("title").is_none());
        assert!(schema.get("$defs").is_none());
        for input in [
            json!({"id":"job"}),
            json!({"id":"job", "tools":null}),
            json!({"id":"job", "tools":[]}),
        ] {
            let normalized = crate::subsystems::tools::schema::check(input.clone(), &schema)
                .expect("schema accepts update");
            let parsed: Update = serde_json::from_value(normalized).expect("typed update");
            assert_eq!(
                parsed.tools,
                match input.get("tools") {
                    None => None,
                    Some(Value::Null) => Some(None),
                    Some(_) => Some(Some(vec![])),
                }
            );
        }
    }
}
