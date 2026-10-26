//! Память и блокнот в контексте и единые функции исполнения их тулов.

use crate::{
    components::Components,
    config::Config,
    db::{Db as _, DbResult, Queries as _},
    shared::Shared,
    subsystems::{
        memory, notes,
        tools::{maintenance, schema::input_schema},
        turns::{
            ctx::Ctx,
            estimate::Estimator,
            log::{self, HygienePayload},
            ports::{BoxFuture, HotBlock, HotCtx, MemoryPort, NotesPort, Toolset, ToolsetCall},
        },
    },
};
use eva_protocol::{
    errors::Aux,
    types::{
        ChatId, ModelId, TurnId,
        memory::tools as mt,
        note::{Mode, tools as nt},
        tool::{Need, ResultForm, ToolContent, ToolDecl},
        turn::TurnView,
    },
    wire::tools::ToolResult,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

#[derive(Default)]
pub struct Integrated {
    read: Mutex<BTreeMap<ChatId, (TurnId, BTreeSet<String>)>>,
}

impl<C: Components> MemoryPort<C> for Integrated {
    fn hot_block<'a>(&'a self, shared: &'a Shared<C>, ctx: HotCtx) -> BoxFuture<'a, HotBlock> {
        Box::pin(async move {
            let view = TurnView {
                chat: ctx.chat,
                actor: None,
                master_turn: ctx.master_turn,
                trusted_chat: ctx.trusted_chat,
                audience: ctx.audience,
            };
            let ratio = Some(Estimator::new(ctx.ratio, &shared.cfg().context).ratio);
            match memory::hot_block(shared, &view, ratio).await {
                Ok(block) => HotBlock {
                    text: Some(block.text),
                    ids: block.ids,
                },
                Err(e) => {
                    ::log::warn!(error:% = e; "memory snapshot unavailable");
                    HotBlock::default()
                }
            }
        })
    }
}

/// Смена режима только на входе в тур, до построения reminder.
pub async fn enter<C: Components>(shared: &Shared<C>, ctx: &Ctx) -> Result<(), Aux> {
    let chat = ctx.chat;
    let cfg = shared.cfg().notes.clone();
    if !cfg.enabled {
        return Ok(());
    }
    let budget = notes::budget::budget(
        shared
            .models()
            .get(&ctx.model)
            .map(|m| m.decl.context_window),
        &cfg,
    );
    let master = ctx.master_turn;
    let model = ctx.model.clone();
    let context = shared.cfg().context.clone();
    let now = shared.now();
    let transition = shared
        .db()
        .write(move |tx| {
            let ratio = Some(Estimator::new(tx.turns().ratio(&model)?, &context).ratio);
            let (_, fill) = notes::observe(tx, chat, budget, ratio)?;
            let entered = notes::state::enter(tx.notes().state(chat)?, fill, master, &cfg, now);
            tx.notes().save_state(chat, &entered.state)?;
            DbResult::Ok(entered.transition)
        })
        .await?;
    if let Some((from, to)) = transition {
        shared
            .bump(
                crate::db::stats::Metric::NotesModeTransition,
                &notes::transition_key(from, to),
            )
            .await;
    }
    Ok(())
}

impl<C: Components> NotesPort<C> for Integrated {
    fn section<'a>(
        &'a self,
        shared: &'a Shared<C>,
        chat: ChatId,
        ratio: Option<f64>,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let cfg = &shared.cfg().notes;
            if !cfg.enabled {
                return None;
            }
            let data = shared
                .db()
                .reader()
                .await
                .ok()?
                .run(move |c| DbResult::Ok((c.notes().list(chat)?, c.notes().state(chat)?)))
                .await
                .ok()?;
            let mode = data.1.map_or(Mode::Inline, |s| s.mode);
            let ratio = Some(Estimator::new(ratio, &shared.cfg().context).ratio);
            Some(format!(
                "{}\n\n{}",
                notes::render::rules(cfg),
                notes::render::section(&data.0, mode, cfg, ratio)
            ))
        })
    }
    fn pressure_line<'a>(
        &'a self,
        shared: &'a Shared<C>,
        chat: ChatId,
        model: ModelId,
    ) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let ctx = shared.turns().get(chat)?.last_ctx()?;
            if !ctx.master_turn || !shared.cfg().notes.enabled {
                return None;
            }
            let cfg = &shared.cfg().notes;
            let budget = notes::budget::budget(
                shared.models().get(&model).map(|m| m.decl.context_window),
                cfg,
            );
            let model_id = model.clone();
            let context = shared.cfg().context.clone();
            let (fill, state, loaded, anchor, compact_at, ratio) = shared
                .db()
                .reader()
                .await
                .ok()?
                .run(move |c| {
                    let segment = c.turns().open_segment(chat)?;
                    let ratio = Some(Estimator::new(c.turns().ratio(&model_id)?, &context).ratio);
                    Result::<_, Aux>::Ok((
                        notes::observe(c, chat, budget, ratio)?.1,
                        c.notes().state(chat)?,
                        segment
                            .map(|s| log::load(c, s.id, None))
                            .transpose()?
                            .flatten(),
                        c.turns().anchor(chat)?,
                        c.turns().compact_at(chat)?,
                        ratio,
                    ))
                })
                .await
                .ok()?;
            let mut state = state.unwrap_or_else(|| notes::state::initial(shared.now()));
            let loaded = loaded?;
            let model = shared.models().get(&model)?.decl.clone();
            let est = Estimator::new(ratio, &shared.cfg().context);
            let rendered = super::render::render(super::render::RenderInput {
                segment: &loaded.snapshot,
                events: &loaded.events,
                caps: &model.caps,
                blobs: &super::render::NoBlobs,
                tz: &shared.day_zone().tz(),
            });
            let window = notes::pressure::Window {
                used: super::estimate::estimate(&est, anchor, &loaded.events, &rendered, None),
                fold_at: super::estimate::compact_threshold(
                    &model,
                    compact_at.unwrap_or(shared.cfg().context.compact_at),
                ),
            };
            let visible = super::gates::step_tools(shared, &ctx)
                .await
                .ok()?
                .specs
                .iter()
                .any(|tool| tool.route == "notes_save");
            let context = notes::pressure::Turn {
                master_turn: ctx.master_turn,
                save_visible: visible,
                segment: loaded.row.id,
                born_of_compaction: loaded.row.summary.is_some(),
            };
            let lines = notes::pressure::reminder(window, context, fill, &mut state, cfg);
            shared
                .db()
                .write(move |tx| tx.notes().save_state(chat, &state))
                .await
                .ok()?;
            let lines: Vec<_> = [lines.window, lines.mode].into_iter().flatten().collect();
            (!lines.is_empty()).then(|| lines.join("\n"))
        })
    }
    fn carried_over<'a>(&'a self, shared: &'a Shared<C>, chat: ChatId) -> BoxFuture<'a, u32> {
        Box::pin(async move {
            match shared.db().reader().await {
                Ok(r) => r
                    .run(move |c| c.notes().list(chat))
                    .await
                    .map_or(0, |n| n.len() as u32),
                Err(_) => 0,
            }
        })
    }
    fn index_mode<'a>(&'a self, shared: &'a Shared<C>, chat: ChatId) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            match shared.db().reader().await {
                Ok(r) => r
                    .run(move |c| c.notes().state(chat))
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|s| s.mode == Mode::Index),
                Err(_) => false,
            }
        })
    }
}

fn decl<I: eva::_priv::schemars::JsonSchema>(
    path: &str,
    description: &str,
    master_only: bool,
    needs: Vec<Need>,
) -> ToolDecl {
    ToolDecl {
        path: path.into(),
        description: description.into(),
        input: input_schema::<I>(),
        master_only,
        trusted_only: false,
        needs,
        ends_turn: false,
        parallel_safe: false,
        waits_on_cancel: false,
        result: ResultForm::Windowed,
        timeout: None,
        idempotent: false,
    }
}
fn text(text: impl Into<String>) -> ToolResult {
    ToolResult::ok(vec![ToolContent::text(text)])
}
fn json<T: serde::Serialize>(value: &T) -> ToolResult {
    text(serde_json::to_string(value).unwrap_or_default())
}
fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, memory::tools::ToolError> {
    serde_json::from_value(value)
        .map_err(|e| memory::tools::ToolError::Refused(format!("invalid input: {e}")))
}

impl<C: Components> Toolset<C> for Integrated {
    fn decls(&self, cfg: &Config) -> Vec<ToolDecl> {
        let mut decls = vec![
            decl::<mt::remember::Args>(
                "memory.remember",
                "Remember a fact, event, procedure or core rule.",
                true,
                vec![],
            ),
            decl::<mt::supersede::Args>(
                "memory.supersede",
                "Replace a memory record with a corrected version.",
                true,
                vec![],
            ),
            decl::<mt::forget::Args>(
                "memory.forget",
                "Archive an outdated memory record with a reason.",
                true,
                vec![],
            ),
            decl::<mt::recall::Args>(
                "memory.recall",
                "Recall relevant memory by query and optional time range.",
                false,
                vec![],
            ),
            decl::<mt::read::Args>(
                "memory.read",
                "Read the complete text of a memory record.",
                false,
                vec![],
            ),
            decl::<mt::read::Args>(
                "memory.history",
                "Read the version history of a memory record.",
                false,
                vec![],
            ),
        ];
        if cfg.notes.enabled {
            decls.extend([
                decl::<nt::save::Args>(
                    "notes.save",
                    "Save or replace a note in this chat's notebook.",
                    true,
                    vec![],
                ),
                decl::<nt::update::Args>(
                    "notes.update",
                    "Update selected fields of a note.",
                    true,
                    vec![],
                ),
                decl::<nt::delete::Args>(
                    "notes.delete",
                    "Delete a spent note from this chat's notebook.",
                    true,
                    vec![],
                ),
                decl::<nt::read::Args>(
                    "notes.read",
                    "Read full note bodies before editing notes shown only in the index.",
                    true,
                    vec![Need::IndexNotes],
                ),
            ]);
        }
        decls.extend(maintenance::decls());
        decls.extend(super::api_tools::decls());
        decls.extend(crate::subsystems::cron::tools::decls());
        decls.extend(crate::subsystems::stats::tools::decls());
        decls.extend(crate::subsystems::modules::tools::decls());
        decls.extend(crate::subsystems::work::hands::decls(cfg));
        decls.extend(crate::subsystems::work::files::decls());
        decls.extend(crate::subsystems::work::agents::decls(cfg));
        decls
    }
    fn call<'a>(
        &'a self,
        shared: &'a Shared<C>,
        call: ToolsetCall<'a>,
    ) -> BoxFuture<'a, Result<ToolResult, Aux>> {
        Box::pin(async move {
            if call.canonical.starts_with("system_modules_") {
                return crate::subsystems::modules::tools::call(shared, call).await;
            }
            if call.canonical.starts_with("cron_") {
                return crate::subsystems::cron::tools::call(shared, call).await;
            }
            if call.canonical.starts_with("stats_") {
                return crate::subsystems::stats::tools::call(shared, call).await;
            }
            if call.canonical.starts_with("chats_") || call.canonical.starts_with("persons_") {
                return super::api_tools::call(shared, call).await;
            }
            if call.canonical.starts_with("agent_") || call.canonical.starts_with("agents_") {
                return crate::subsystems::work::agents::call(shared, call).await;
            }
            if call.canonical.starts_with("files_") {
                return crate::subsystems::work::files::call(shared, call).await;
            }
            if call.canonical.starts_with("fs_")
                || call.canonical.starts_with("shell_")
                || call.canonical.starts_with("hands_grants_")
            {
                return crate::subsystems::work::hands::call(shared, call).await;
            }
            if call.canonical.starts_with("notes_") {
                return self.notes_call(shared, call).await;
            }
            if matches!(call.canonical, "tools_group" | "tools_override") {
                return maintenance::call(shared, call.canonical, call.input).await;
            }
            let ctx = memory::tools::TurnMemory {
                shared,
                view: call.view,
                surface: Some(&call.ctx.surface),
                run_id: call.ctx.run_id.as_ref(),
                hot_ids: call.hot_ids,
                ratio: call.ratio,
            };
            let result: Result<ToolResult, memory::tools::ToolError> = async {
                Ok(match call.canonical {
                    "memory_remember" => text(memory::tools::remember_text(
                        &memory::tools::remember(&ctx, parse(call.input)?).await?,
                    )),
                    "memory_supersede" => text(memory::tools::supersede_text(
                        &memory::tools::supersede(&ctx, parse(call.input)?).await?,
                    )),
                    "memory_forget" => text(memory::tools::forget_text(
                        &memory::tools::forget(&ctx, parse(call.input)?).await?,
                        shared.now(),
                    )),
                    "memory_recall" => text(memory::tools::recall_text(
                        &memory::tools::recall(&ctx, parse(call.input)?).await?,
                    )),
                    "memory_read" => json(&memory::tools::read(&ctx, parse(call.input)?).await?),
                    "memory_history" => {
                        json(&memory::tools::history(&ctx, parse(call.input)?).await?)
                    }
                    name => {
                        return Err(memory::tools::ToolError::Fatal(Aux::Internal(format!(
                            "soul tool {name} has no handler"
                        ))));
                    }
                })
            }
            .await;
            match result {
                Ok(result) => Ok(result),
                Err(memory::tools::ToolError::Refused(e)) => Ok(ToolResult::error(e)),
                Err(memory::tools::ToolError::Fatal(e)) => Err(e),
            }
        })
    }
}

impl Integrated {
    async fn notes_call<C: Components>(
        &self,
        shared: &Shared<C>,
        call: ToolsetCall<'_>,
    ) -> Result<ToolResult, Aux> {
        let chat = call.ctx.chat;
        let turn = call.ctx.turn_id.clone();
        let mut read = self
            .read
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&chat)
            .filter(|(id, _)| id == &turn)
            .map(|(_, read)| read.clone())
            .unwrap_or_default();
        let cfg = shared.cfg().notes.clone();
        let model = shared.turns().get(chat).and_then(|slot| {
            slot.last_request()
                .map(|request| request.model)
                .or_else(|| slot.last_ctx().map(|ctx| ctx.model))
        });
        let budget = notes::budget::budget(
            model
                .as_ref()
                .and_then(|m| shared.models().get(m).map(|m| m.decl.context_window)),
            &cfg,
        );
        let now = shared.now();
        let ratio = Some(Estimator::new(call.ratio, &shared.cfg().context).ratio);
        let canonical = call.canonical.to_owned();
        let input = call.input;
        let result = shared
            .db()
            .write(move |tx| -> Result<_, notes::tools::Error> {
                let mode = tx.notes().state(chat)?.map_or(Mode::Inline, |s| s.mode);
                let mut ctx = notes::tools::Ctx {
                    chat,
                    mode,
                    read_this_turn: &mut read,
                    ratio,
                    budget,
                    now,
                };
                macro_rules! args {
                    () => {
                        serde_json::from_value(input).map_err(|e| {
                            notes::tools::Error::Rejected(notes::tools::Rejected {
                                reason: "invalid",
                                message: format!("invalid input: {e}"),
                            })
                        })?
                    };
                }
                let (result, hygiene) = match canonical.as_str() {
                    "notes_save" => {
                        let result = notes::tools::save(tx, &cfg, &mut ctx, args!())?;
                        (json(&result.ok), Some(result.event))
                    }
                    "notes_update" => {
                        let result = notes::tools::update(tx, &cfg, &mut ctx, args!())?;
                        (json(&result.ok), Some(result.event))
                    }
                    "notes_delete" => {
                        let result = notes::tools::delete(tx, &cfg, &mut ctx, args!())?;
                        (json(&result.ok), Some(result.event))
                    }
                    "notes_read" => (
                        text(notes::tools::read(tx, &cfg, &mut ctx, args!())?.text),
                        None,
                    ),
                    _ => (ToolResult::error("unknown notes tool"), None),
                };
                Ok((result, hygiene, read))
            })
            .await;
        match result {
            Ok((result, hygiene, read)) => {
                self.read
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(chat, (turn, read));
                if let Some(hygiene) = hygiene {
                    log::append_hygiene(shared, chat, HygienePayload::NoteUpdated(hygiene)).await?;
                }
                Ok(result)
            }
            Err(notes::tools::Error::Rejected(e)) => {
                shared
                    .bump(crate::db::stats::Metric::NotesRejected, e.reason)
                    .await;
                Ok(ToolResult::error(e.message))
            }
            Err(notes::tools::Error::Db(e)) => Err(e.into()),
        }
    }
}
