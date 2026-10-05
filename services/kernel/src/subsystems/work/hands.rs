//! Тулы рук исполняются только через Machine endpoint'а чата.

use crate::{
    components::Components,
    config::Config,
    db::{Db as _, DbResult, Queries as _, hands_grants::GrantFilter},
    hands::{Endpoint, Machine},
    shared::Shared,
    subsystems::{
        tools::schema::input_schema,
        turns::{pending, ports::ToolsetCall},
    },
};
use eva::{data, time::HumanDuration};
use eva_core::{CallStep as _, service::hands_grants::HandsGrants as _};
use eva_hands::{Error, Kind, Limits};
use eva_protocol::{
    errors::Aux,
    requests::hands_grants,
    types::{
        SegmentId,
        hands::GrantKind,
        tool::{Need, ResultForm, ToolContent, ToolDecl},
    },
    wire::tools::ToolResult,
};
use serde_json::{Value, json};
use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[data]
pub struct Read {
    pub path: String,
    pub offset: Option<u32>,
    pub limit: Option<u32>,
}
#[data]
pub struct Find {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    pub limit: Option<u32>,
}
#[data]
pub struct Write {
    pub path: String,
    pub content: String,
}
#[data]
pub struct Edit {
    pub path: String,
    pub old: String,
    pub new: String,
}
#[data]
pub struct Patch {
    pub patch: String,
}
#[data]
pub struct Remove {
    pub path: String,
}
#[data]
pub struct Rename {
    pub from: String,
    pub to: String,
}
#[data]
pub struct Exec {
    pub command: String,
    pub timeout: Option<HumanDuration>,
    pub cwd: Option<String>,
}

#[data]
pub struct ListGrants {}

pub fn decls(cfg: &Config) -> Vec<ToolDecl> {
    let mut out: Vec<ToolDecl> = [
        (
            "fs.read",
            "Read a file in line windows.",
            input_schema::<Read>(),
            false,
            true,
        ),
        (
            "fs.find",
            "Find matching lines in files using a regular expression and optional glob.",
            input_schema::<Find>(),
            false,
            true,
        ),
        (
            "fs.write",
            "Write a text file; read existing files first.",
            input_schema::<Write>(),
            true,
            false,
        ),
        (
            "fs.edit",
            "Replace exactly one matching text block in a previously read file.",
            input_schema::<Edit>(),
            true,
            false,
        ),
        (
            "fs.patch",
            "Apply a patch after validating every file and hunk.",
            input_schema::<Patch>(),
            true,
            false,
        ),
        (
            "fs.remove",
            "Remove a file or directory.",
            input_schema::<Remove>(),
            true,
            false,
        ),
        (
            "fs.rename",
            "Rename a file or directory.",
            input_schema::<Rename>(),
            true,
            false,
        ),
        (
            "shell.exec",
            "Run a shell command and return its exit code, signal, timeout status and output.",
            input_schema::<Exec>(),
            true,
            true,
        ),
    ]
    .into_iter()
    .map(|(path, description, input, writes, reader)| ToolDecl {
        path: path.into(),
        description: description.into(),
        input,
        master_only: true,
        trusted_only: true,
        needs: vec![Need::Hands, Need::Private],
        ends_turn: false,
        parallel_safe: !writes,
        waits_on_cancel: writes,
        result: if reader {
            ResultForm::Uncapped
        } else {
            ResultForm::Windowed
        },
        timeout: Some(if path == "shell.exec" {
            cfg.work.shell_timeout_max
        } else {
            cfg.work.fs_timeout
        }),
        idempotent: false,
    })
    .collect();
    for (path, description, input) in [
        (
            "hands.grants.list",
            "List permanent working-machine permissions for this surface.",
            input_schema::<ListGrants>(),
        ),
        (
            "hands.grants.revoke",
            "Revoke a permanent working-machine permission by id.",
            input_schema::<hands_grants::revoke::Args>(),
        ),
    ] {
        out.push(ToolDecl {
            path: path.into(),
            description: description.into(),
            input,
            master_only: true,
            trusted_only: true,
            needs: Vec::new(),
            ends_turn: false,
            parallel_safe: false,
            waits_on_cancel: false,
            result: ResultForm::Windowed,
            timeout: None,
            idempotent: false,
        });
    }
    out
}

struct Target<M> {
    endpoint: Arc<Endpoint<M>>,
    cwd: Option<String>,
    segment: SegmentId,
    limits: Limits,
    timeout: Duration,
}

async fn timed<T>(
    timeout: Duration,
    future: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    tokio::time::timeout(timeout, future)
        .await
        .unwrap_or_else(|_| {
            Err(Error::Failed(eva_hands::Failure::new(
                eva_hands::Code::Timeout,
                "operation timed out",
            )))
        })
}

fn ok(text: impl Into<String>) -> ToolResult {
    ToolResult::ok(vec![ToolContent::text(text)])
}
fn failed(name: &str, error: Error) -> ToolResult {
    match error {
        Error::Unavailable(message) => ToolResult::error(format!(
            "the working machine {name:?} is unreachable: {message}"
        )),
        Error::Failed(failure) => ToolResult::error(failure.message),
    }
}

pub fn path(cwd: Option<&str>, input: &str) -> Result<String, String> {
    let input = Path::new(input);
    let absolute = if input.is_absolute() {
        input.to_path_buf()
    } else {
        Path::new(cwd.ok_or("no workdir")?).join(input)
    };
    if !absolute.is_absolute() {
        return Err("workdir must be absolute".into());
    }
    let mut out = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            part => out.push(part.as_os_str()),
        }
    }
    Ok(out.to_string_lossy().into_owned())
}

fn check_root(path: &str, target: &Target<impl Machine>) -> Result<(), String> {
    if target.limits.roots.is_empty()
        || target
            .limits
            .roots
            .iter()
            .any(|root| Path::new(path).starts_with(root))
    {
        Ok(())
    } else {
        Err(format!(
            "endpoint {:?}: path is outside limits.roots",
            target.endpoint.name
        ))
    }
}

/// Лимиты демона лексические: проверяем ссылки до чтения и мутации пути.
pub async fn no_symlinks<M: Machine>(
    machine: &M,
    limits: &Limits,
    path: &str,
    timeout: Duration,
) -> Result<(), Error> {
    let path = Path::new(path);
    let base = limits
        .roots
        .iter()
        .filter(|root| path.starts_with(root))
        .min_by_key(|root| root.components().count())
        .map_or(Path::new("/"), PathBuf::as_path);
    let mut prefix = base.to_path_buf();
    let mut prefixes = vec![prefix.clone()];
    if let Ok(rest) = path.strip_prefix(base) {
        for component in rest.components() {
            prefix.push(component);
            prefixes.push(prefix.clone());
        }
    }
    for prefix in prefixes {
        match timed(timeout, machine.stat(None, &prefix.to_string_lossy())).await {
            Ok(entry) if entry.kind == Kind::Symlink => {
                return Err(Error::Failed(eva_hands::Failure::new(
                    eva_hands::Code::Denied,
                    format!(
                        "symbolic links are not supported in working paths: {}",
                        prefix.display()
                    ),
                )));
            }
            Ok(_) => {}
            Err(Error::Failed(failure)) if failure.code == eva_hands::Code::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

async fn permitted<C: Components>(
    shared: &Shared<C>,
    call: &ToolsetCall<'_>,
    target: &Target<C::Hands>,
    kind: GrantKind,
    values: &[String],
) -> Result<pending::PermissionOutcome, Aux> {
    let endpoint = target.endpoint.name.clone();
    let filter = GrantFilter {
        endpoint: Some(endpoint.clone()),
        surface: Some(call.ctx.surface.clone()),
        chat: None,
    };
    let grants = shared
        .db()
        .reader()
        .await?
        .run(move |c| c.hands_grants().list(&filter))
        .await?;
    let covered = |value: &str| {
        (kind == GrantKind::Read
            && target
                .cwd
                .as_deref()
                .is_some_and(|cwd| Path::new(value).starts_with(cwd)))
            || shared
                .access()
                .session_allows(shared, &endpoint, call.ctx.chat, kind, value)
            || grants.iter().any(|g| {
                g.kind == kind
                    && g.chat.is_none_or(|chat| chat == call.ctx.chat)
                    && super::matches_prefix(kind, &g.prefix, value)
            })
    };
    if values.iter().all(|value| covered(value)) {
        return Ok(pending::PermissionOutcome::Allowed);
    }
    let payload = if kind == GrantKind::Shell {
        json!({"endpoint": endpoint, "kind": kind, "command": values.first()})
    } else {
        json!({"endpoint": endpoint, "kind": kind, "paths": values})
    };
    pending::permission(shared, call.ctx, call.call_id, payload).await
}

async fn read_all<M: Machine>(target: &Target<M>, path: &str) -> Result<Vec<u8>, Error> {
    no_symlinks(
        &target.endpoint.machine,
        &target.limits,
        path,
        target.timeout,
    )
    .await?;
    let (total, bytes) = timed(
        target.timeout,
        target
            .endpoint
            .machine
            .read(None, path, 0, Some(target.limits.max_read)),
    )
    .await?;
    if total > bytes.len() as u64 {
        return Err(Error::Failed(eva_hands::Failure {
            code: eva_hands::Code::TooLarge,
            message: format!(
                "file exceeds endpoint max_read ({} bytes)",
                target.limits.max_read
            ),
        }));
    }
    Ok(bytes)
}

pub fn window(
    text: &str,
    offset: u32,
    limit: u32,
    tokens: u32,
    ratio: f64,
) -> Result<String, String> {
    let lines: Vec<_> = text.lines().collect();
    if lines.is_empty() {
        if offset > 1 {
            return Err(format!("offset {offset} is past the end: 0 lines"));
        }
        return Ok("[lines 0-0 of 0]\n".to_owned());
    }
    let from = offset.max(1) as usize;
    if from > lines.len().max(1) {
        return Err(format!(
            "offset {from} is past the end: {} lines",
            lines.len()
        ));
    }
    let mut end = (from - 1)
        .saturating_add(limit.max(1) as usize)
        .min(lines.len());
    let bytes = (f64::from(tokens) * ratio.max(1.0)) as usize;
    let mut body = lines[from.saturating_sub(1).min(lines.len())..end].join("\n");
    while body.len() > bytes && end > from {
        end -= 1;
        body = lines[from - 1..end].join("\n");
    }
    if body.len() > bytes {
        let mut cut = bytes.min(body.len());
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
        body.push_str(" [line truncated]");
    }
    let continuation = if end < lines.len() {
        format!(" — read on with offset {}", end + 1)
    } else {
        String::new()
    };
    Ok(format!(
        "[lines {from}-{end} of {}{continuation}]\n{body}",
        lines.len()
    ))
}

pub async fn call<C: Components>(
    shared: &Shared<C>,
    call: ToolsetCall<'_>,
) -> Result<ToolResult, Aux> {
    if call.canonical.starts_with("hands_grants_") {
        let mut session = crate::session::for_tool(shared, call.ctx.chat);
        if call.canonical == "hands_grants_list" {
            return Ok(
                match session
                    .hands_grants()
                    .list()
                    .call(hands_grants::list::Args {
                        surface: Some(call.ctx.surface.clone()),
                        ..Default::default()
                    })
                    .await
                {
                    Ok(list) => {
                        ok(serde_json::to_string(&list)
                            .map_err(|e| Aux::Internal(e.to_string()))?)
                    }
                    Err(error) => ToolResult::error(error.to_string()),
                },
            );
        }
        let args = match serde_json::from_value(call.input) {
            Ok(args) => args,
            Err(error) => return Ok(ToolResult::error(format!("invalid input: {error}"))),
        };
        return Ok(match session.hands_grants().revoke().call(args).await {
            Ok(_) => ok("{\"revoked\":true}"),
            Err(error) => ToolResult::error(error.to_string()),
        });
    }
    let chat = call.ctx.chat;
    let (row, segment) = shared
        .db()
        .reader()
        .await?
        .run(move |c| DbResult::Ok((c.chats().get(chat)?, c.turns().open_segment(chat)?)))
        .await?;
    let Some(row) = row else {
        return Ok(ToolResult::error("chat not found"));
    };
    let Some(segment) = segment else {
        return Err(Aux::Internal("hands call has no segment".into()));
    };
    let endpoint = row
        .workdir_on
        .as_ref()
        .or(shared.cfg().hands.default_endpoint.as_ref());
    let Some(endpoint) = endpoint.and_then(|name| shared.hands().get(name)).cloned() else {
        return Ok(ToolResult::error("working machine is unavailable"));
    };
    let limits = match endpoint.limits().await {
        Ok(limits) => limits,
        Err(error) => return Ok(failed(&endpoint.name, error)),
    };
    let target = Target {
        endpoint,
        cwd: row.workdir,
        segment: segment.id,
        limits,
        timeout: shared.cfg().work.fs_timeout.0,
    };
    let outcome = execute(shared, &call, &target).await;
    match outcome {
        Ok(result) => Ok(result),
        Err(CallError::Input(message)) => Ok(ToolResult::error(message)),
        Err(CallError::Hands(error)) => {
            if error.is_unavailable() {
                target.endpoint.forget_limits().await;
            }
            Ok(failed(&target.endpoint.name, error))
        }
        Err(CallError::Fatal(error)) => Err(error),
    }
}

enum CallError {
    Input(String),
    Hands(Error),
    Fatal(Aux),
}
impl From<Error> for CallError {
    fn from(e: Error) -> Self {
        Self::Hands(e)
    }
}
impl From<String> for CallError {
    fn from(e: String) -> Self {
        Self::Input(e)
    }
}
impl From<Aux> for CallError {
    fn from(e: Aux) -> Self {
        Self::Fatal(e)
    }
}
fn parse<T: serde::de::DeserializeOwned>(input: &Value) -> Result<T, CallError> {
    serde_json::from_value(input.clone())
        .map_err(|e| CallError::Input(format!("invalid input: {e}")))
}

async fn authorize<C: Components>(
    shared: &Shared<C>,
    call: &ToolsetCall<'_>,
    target: &Target<C::Hands>,
    kind: GrantKind,
    paths: &[String],
) -> Result<(), CallError> {
    for path in paths {
        if kind != GrantKind::Shell {
            check_root(path, target)?;
            no_symlinks(
                &target.endpoint.machine,
                &target.limits,
                path,
                target.timeout,
            )
            .await?;
        }
    }
    let outcome = permitted(shared, call, target, kind, paths).await?;
    if shared
        .turns()
        .get(call.ctx.chat)
        .and_then(|slot| slot.live())
        .is_some_and(|live| live.cancel.is_cancelled())
    {
        return Err(CallError::Input("permission cancelled".into()));
    }
    match outcome {
        pending::PermissionOutcome::Allowed => {
            if kind != GrantKind::Shell {
                for path in paths { no_symlinks(&target.endpoint.machine, &target.limits, path, target.timeout).await?; }
            }
            Ok(())
        },
        pending::PermissionOutcome::Denied => Err("permission denied".to_owned().into()),
        pending::PermissionOutcome::Expired => Err("permission timed out".to_owned().into()),
        pending::PermissionOutcome::Cancelled => Err("permission cancelled".to_owned().into()),
        pending::PermissionOutcome::Unavailable => Err("permission unavailable: no master client is connected — ask the master for allow_always".to_owned().into()),
    }
}

async fn guard<C: Components>(
    shared: &Shared<C>,
    call: &ToolsetCall<'_>,
    target: &Target<C::Hands>,
    path: &str,
) -> Result<(), CallError> {
    no_symlinks(
        &target.endpoint.machine,
        &target.limits,
        path,
        target.timeout,
    )
    .await?;
    match timed(target.timeout, target.endpoint.machine.stat(None, path)).await {
        Ok(_)
            if !shared.access().was_read(
                call.ctx.chat,
                target.segment,
                &target.endpoint.name,
                path,
            ) =>
        {
            Err(format!("read it first: fs_read {path}").into())
        }
        Ok(_) => Ok(()),
        Err(Error::Failed(f)) if f.code == eva_hands::Code::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

async fn execute<C: Components>(
    shared: &Shared<C>,
    call: &ToolsetCall<'_>,
    target: &Target<C::Hands>,
) -> Result<ToolResult, CallError> {
    let machine = &target.endpoint.machine;
    let resolve = |input: &str| -> Result<String, CallError> {
        let p = path(target.cwd.as_deref(), input)?;
        check_root(&p, target)?;
        Ok(p)
    };
    let cfg = shared.cfg();
    let ratio = call.ratio.unwrap_or(cfg.context.ratio_default);
    match call.canonical {
        "fs_read" => {
            let args: Read = parse(&call.input)?;
            let path = resolve(&args.path)?;
            authorize(
                shared,
                call,
                target,
                GrantKind::Read,
                std::slice::from_ref(&path),
            )
            .await?;
            let bytes = read_all(target, &path).await?;
            shared.access().record_read(
                call.ctx.chat,
                target.segment,
                &target.endpoint.name,
                &path,
            );
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| CallError::Input(format!("binary file, {} bytes", bytes.len())))?;
            Ok(ok(window(
                text,
                args.offset.unwrap_or(1),
                args.limit.unwrap_or(cfg.work.read_default_lines),
                cfg.work.reader_result_tokens,
                ratio,
            )?))
        }
        "fs_write" | "fs_edit" => {
            let (path, content, preview) = if call.canonical == "fs_write" {
                let args: Write = parse(&call.input)?;
                (resolve(&args.path)?, args.content, None)
            } else {
                let args: Edit = parse(&call.input)?;
                let path = resolve(&args.path)?;
                guard(shared, call, target, &path).await?;
                let bytes = read_all(target, &path).await?;
                let text = String::from_utf8(bytes)
                    .map_err(|_| CallError::Input("cannot edit a binary file".into()))?;
                if args.old.is_empty() {
                    return Err("old must not be empty".to_owned().into());
                }
                let count = text.matches(&args.old).count();
                if count != 1 {
                    return Err(if count == 0 {
                        "not found".into()
                    } else {
                        format!("ambiguous: {count} matches")
                    }
                    .into());
                }
                let first_line = text.find(&args.old).map_or(1, |offset| {
                    text[..offset].bytes().filter(|&byte| byte == b'\n').count() as u32 + 1
                });
                (
                    path,
                    text.replacen(&args.old, &args.new, 1),
                    Some(first_line.saturating_sub(3).max(1)),
                )
            };
            guard(shared, call, target, &path).await?;
            if content.len() as u64 > target.limits.max_write {
                return Err(format!(
                    "endpoint {:?}: content exceeds max_write",
                    target.endpoint.name
                )
                .into());
            }
            authorize(
                shared,
                call,
                target,
                GrantKind::Write,
                std::slice::from_ref(&path),
            )
            .await?;
            timed(
                target.timeout,
                machine.write(None, &path, content.as_bytes(), true, None),
            )
            .await?;
            let mut result = format!("wrote {} lines to {path}", content.lines().count());
            if let Some(from) = preview {
                result.push('\n');
                result.push_str(&window(
                    &content,
                    from,
                    7,
                    cfg.work.reader_result_tokens,
                    ratio,
                )?);
            }
            Ok(ok(result))
        }
        "fs_remove" => {
            let args: Remove = parse(&call.input)?;
            let path = resolve(&args.path)?;
            authorize(
                shared,
                call,
                target,
                GrantKind::Write,
                std::slice::from_ref(&path),
            )
            .await?;
            timed(target.timeout, machine.remove(None, &path, true)).await?;
            Ok(ok(format!("removed {path}")))
        }
        "fs_rename" => {
            let args: Rename = parse(&call.input)?;
            let from = resolve(&args.from)?;
            let to = resolve(&args.to)?;
            authorize(
                shared,
                call,
                target,
                GrantKind::Write,
                &[from.clone(), to.clone()],
            )
            .await?;
            timed(target.timeout, machine.rename(None, &from, &to)).await?;
            Ok(ok(format!("renamed {from} → {to}")))
        }
        "shell_exec" => {
            let args: Exec = parse(&call.input)?;
            if !target.limits.exec {
                return Err(
                    format!("endpoint {:?}: limits.exec is false", target.endpoint.name).into(),
                );
            }
            let timeout = args.timeout.unwrap_or(cfg.work.shell_timeout).0;
            if timeout > cfg.work.shell_timeout_max.0
                || timeout.as_secs() > target.limits.exec_max_timeout_secs
            {
                return Err("timeout exceeds shell_timeout_max or endpoint limit"
                    .to_owned()
                    .into());
            }
            let cwd = args
                .cwd
                .as_deref()
                .map(&resolve)
                .transpose()?
                .or_else(|| target.cwd.clone());
            if let Some(cwd) = &cwd {
                check_root(cwd, target)?;
                no_symlinks(machine, &target.limits, cwd, target.timeout).await?;
            }
            authorize(
                shared,
                call,
                target,
                GrantKind::Shell,
                std::slice::from_ref(&args.command),
            )
            .await?;
            if let Some(cwd) = &cwd {
                no_symlinks(machine, &target.limits, cwd, target.timeout).await?;
            }
            let cancel = shared
                .turns()
                .get(call.ctx.chat)
                .and_then(|slot| slot.live())
                .filter(|live| live.turn == call.ctx.turn_id)
                .map(|live| live.cancel);
            let output = machine
                .exec(
                    cwd.as_deref(),
                    &args.command,
                    u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                    Vec::new(),
                )
                .await?;
            let code = output
                .code
                .map_or_else(|| "none".to_owned(), |code| code.to_string());
            let signal = output.signal.as_deref().unwrap_or("none");
            let mut status = format!("exit {code} signal {signal}");
            if let Some(timeout_ms) = output.timed_out_ms {
                status.push_str(&format!(
                    " timed out after {:?}",
                    Duration::from_millis(timeout_ms)
                ));
            }
            if cancel.is_some_and(|cancel| cancel.is_cancelled()) {
                status.push_str(" cancelled");
            }
            let max_bytes = (f64::from(cfg.work.reader_result_tokens) * ratio) as usize;
            let tail = |s: &str| {
                let mut from = s.len().saturating_sub(max_bytes / 2);
                while !s.is_char_boundary(from) {
                    from += 1;
                }
                let total = s.lines().count();
                let first = if total == 0 {
                    0
                } else {
                    total.saturating_sub(s[from..].lines().count()) + 1
                };
                format!("[tail: lines {}-{total} of {total}]\n{}", first, &s[from..])
            };
            Ok(ok(format!(
                "{status}\n{}\n[stderr]\n{}",
                tail(&output.stdout),
                tail(&output.stderr)
            )))
        }
        "fs_find" => {
            let args: Find = parse(&call.input)?;
            let pattern = regex::Regex::new(&args.pattern)
                .map_err(|e| CallError::Input(format!("invalid regex: {e}")))?;
            let root = resolve(args.path.as_deref().unwrap_or("."))?;
            authorize(
                shared,
                call,
                target,
                GrantKind::Read,
                std::slice::from_ref(&root),
            )
            .await?;
            let glob = args.glob.as_deref().map(glob_regex).transpose()?;
            let max =
                u64::from(args.limit.unwrap_or(100).max(1)).min(target.limits.max_hits) as usize;
            let mut pending = vec![root.clone()];
            let mut hits = Vec::new();
            let mut omitted = 0_u32;
            let mut omitted_by_dir = std::collections::BTreeMap::<String, u32>::new();
            let mut truncated_listing = false;
            while let Some(path) = pending.pop() {
                let entry = timed(target.timeout, machine.stat(None, &path)).await?;
                if entry.kind == Kind::Dir {
                    let (mut entries, cut) =
                        timed(target.timeout, machine.list(None, &path)).await?;
                    entries.sort_by(|a, b| a.name.cmp(&b.name));
                    truncated_listing |= cut;
                    for entry in entries
                        .into_iter()
                        .rev()
                        .filter(|e| matches!(e.kind, Kind::File | Kind::Dir))
                    {
                        pending.push(format!("{path}/{}", entry.name));
                    }
                } else if entry.kind == Kind::File
                    && glob.as_ref().is_none_or(|glob| glob.is_match(&path))
                {
                    let bytes = read_all(target, &path).await?;
                    if let Ok(text) = std::str::from_utf8(&bytes) {
                        for (line, text) in text
                            .lines()
                            .enumerate()
                            .filter(|(_, line)| pattern.is_match(line))
                        {
                            if hits.len() < max {
                                hits.push(format!("{path}:{}: {text}", line + 1));
                            } else {
                                omitted = omitted.saturating_add(1);
                                let rel = Path::new(&path).strip_prefix(&root).ok();
                                let mut parts = rel.into_iter().flat_map(Path::components);
                                let first = parts.next();
                                let dir = if parts.next().is_some() {
                                    first.map_or_else(
                                        || root.clone(),
                                        |part| {
                                            Path::new(&root)
                                                .join(part.as_os_str())
                                                .to_string_lossy()
                                                .into_owned()
                                        },
                                    )
                                } else {
                                    root.clone()
                                };
                                let count = omitted_by_dir.entry(dir).or_default();
                                *count = count.saturating_add(1);
                            }
                        }
                    }
                }
            }
            if omitted > 0 {
                let summary = omitted_by_dir
                    .into_iter()
                    .map(|(dir, count)| format!("{dir}/ {count}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                hits.push(format!("[{omitted} more: {summary} — narrow path or glob]"));
            }
            if truncated_listing {
                hits.push("[directory listing truncated — narrow path]".to_owned());
            }
            Ok(ok(window(
                &hits.join("\n"),
                1,
                hits.len().max(1) as u32,
                cfg.work.reader_result_tokens,
                ratio,
            )?))
        }
        "fs_patch" => {
            let args: Patch = parse(&call.input)?;
            let changes = super::patch::parse(&args.patch)?;
            let mut staged = Vec::new();
            let mut paths = Vec::new();
            let mut pending_content = std::collections::BTreeMap::<String, String>::new();
            for change in changes {
                let path = resolve(change.path())?;
                let planned = match change {
                    super::patch::Change::Add { content, .. } => {
                        if pending_content.contains_key(&path) {
                            return Err(format!("file already exists in patch: {path}").into());
                        }
                        match timed(target.timeout, machine.stat(None, &path)).await {
                            Err(Error::Failed(f)) if f.code == eva_hands::Code::NotFound => {}
                            Err(e) => return Err(e.into()),
                            Ok(_) => return Err(format!("file already exists: {path}").into()),
                        }
                        pending_content.insert(path.clone(), content.clone());
                        super::patch::Prepared::Write {
                            path: path.clone(),
                            content,
                        }
                    }
                    super::patch::Change::Delete { .. } => {
                        timed(target.timeout, machine.stat(None, &path)).await?;
                        pending_content.remove(&path);
                        super::patch::Prepared::Delete { path: path.clone() }
                    }
                    super::patch::Change::Update { hunks, to, .. } => {
                        let text = if let Some(text) = pending_content.get(&path) {
                            text.clone()
                        } else {
                            let bytes = read_all(target, &path).await?;
                            String::from_utf8(bytes)
                                .map_err(|_| CallError::Input("cannot patch binary file".into()))?
                        };
                        let content = super::patch::apply(&text, &hunks)?;
                        let to = to.map(|p| resolve(&p)).transpose()?;
                        if let Some(to) = &to {
                            paths.push(to.clone());
                        }
                        pending_content
                            .insert(to.as_ref().unwrap_or(&path).clone(), content.clone());
                        super::patch::Prepared::Update {
                            path: path.clone(),
                            content,
                            to,
                        }
                    }
                };
                if planned
                    .content()
                    .is_some_and(|text| text.len() as u64 > target.limits.max_write)
                {
                    return Err("patch exceeds endpoint max_write".to_owned().into());
                }
                paths.push(path);
                staged.push(planned);
            }
            authorize(shared, call, target, GrantKind::Write, &paths).await?;
            let mut report = Vec::new();
            for change in staged {
                match change {
                    super::patch::Prepared::Write { path, content } => {
                        timed(
                            target.timeout,
                            machine.write(None, &path, content.as_bytes(), true, None),
                        )
                        .await?;
                        report.push(format!("applied {path}"));
                    }
                    super::patch::Prepared::Delete { path } => {
                        timed(target.timeout, machine.remove(None, &path, false)).await?;
                        report.push(format!("applied {path}"));
                    }
                    super::patch::Prepared::Update { path, content, to } => {
                        timed(
                            target.timeout,
                            machine.write(None, &path, content.as_bytes(), false, None),
                        )
                        .await?;
                        if let Some(to) = to {
                            timed(target.timeout, machine.rename(None, &path, &to)).await?;
                        }
                        report.push(format!("applied {path}"));
                    }
                }
            }
            Ok(ok(report.join("\n")))
        }
        _ => Err(CallError::Fatal(Aux::Internal(format!(
            "unknown hands tool {}",
            call.canonical
        )))),
    }
}

fn glob_regex(glob: &str) -> Result<regex::Regex, String> {
    let mut pattern = String::from("(?:^|/)");
    for c in glob.chars() {
        match c {
            '*' => pattern.push_str(".*"),
            '?' => pattern.push('.'),
            c => pattern.push_str(&regex::escape(&c.to_string())),
        }
    }
    pattern.push('$');
    regex::Regex::new(&pattern).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_and_windows_have_honest_boundaries() {
        assert_eq!(path(Some("/work/tree"), "../file").unwrap(), "/work/file");
        assert!(path(None, "relative").is_err());
        assert_eq!(
            window("a\nb\nc\nd", 2, 2, 100, 4.0).unwrap(),
            "[lines 2-3 of 4 — read on with offset 4]\nb\nc"
        );
        assert!(window("a\nb", 3, 1, 100, 4.0).is_err());
    }
}
