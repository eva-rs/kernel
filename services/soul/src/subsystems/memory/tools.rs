//! Контракты тулов группы `memory` (`03 §6`, `§7`) как функции компонента:
//! гейт, лимиты, дедуп и провенанс здесь — любая дверь проходит их.
//!
//! `turns::ports::Toolset` передаёт `TurnMemory`; изменения core через
//! `changes::track` дописывают гигиену открытых сегментов в той же транзакции.

use std::collections::BTreeMap;

use eva::time::Timestamp;
use eva_protocol::{
    errors::Aux,
    types::{
        MemoryId, ModelId, RunId, SurfaceId,
        memory::{
            Archive, ArchiveReason, Kind, Percent, Record, Version, Visibility,
            tools::{Action, Neighbour, forget, read, recall, remember, supersede},
        },
        turn::TurnView,
    },
    wire::events::{Event, MemoryChanged, MemoryOp},
};

use crate::{
    components::Components,
    config::MemoryConfig,
    convert,
    db::{
        Db as _, DbError, Queries as _,
        memory::{MemoryRow, Meta, NewMemory, Scope},
        stats::Metric,
    },
    handling::inference::estimate_bytes,
    markers::sanitize_inline,
    shared::Shared,
    subsystems::memory::{
        embed::Embedder,
        recall as recall_impl,
        text::{self, is_slug, jaccard, rrf, trigrams},
        visibility::{Gate, WITHHELD, default_visibility},
    },
};

pub const MASTER_ONLY: &str = "memory changes are master-only";
pub const TRUSTED_ONLY: &str = "core/procedure are written only from trusted chats";
/// Маркер reminder не-master тура (`03 §7`).
pub const NON_MASTER_NOTE: &str =
    "memory changes are master-only; say so, don't pretend the tool doesn't exist";
/// Пометка соседей в ответе `remember`: момент записи — единственный, когда противоречие видно.
pub const RECONCILE: &str = "reconcile now: memory_supersede or memory_forget";
pub use crate::subsystems::memory::recall::NOTHING_NEW;

pub const NAME_MAX: usize = 48;
pub const TAGS_MAX: usize = 8;
pub const TAG_MAX: usize = 24;

/// Тур, из которого зовут тул: провенанс записи и гейты.
pub struct TurnMemory<'a, C: Components> {
    pub shared: &'a Shared<C>,
    pub view: &'a TurnView,
    pub surface: Option<&'a SurfaceId>,
    pub run_id: Option<&'a RunId>,
    /// `hot_memory_ids` сегмента: recall их не возвращает.
    pub hot_ids: &'a [MemoryId],
    /// Калиброванный ratio модели чата (`01 §9`).
    pub ratio: Option<f64>,
}

/// `Refused` — `tool_result{is_error}` с причиной; `Fatal` — беда soul.
#[derive(Debug)]
pub enum ToolError {
    Refused(String),
    Fatal(Aux),
}

impl From<Aux> for ToolError {
    fn from(e: Aux) -> Self {
        Self::Fatal(e)
    }
}

impl From<DbError> for ToolError {
    fn from(e: DbError) -> Self {
        Self::Fatal(e.into())
    }
}

fn refuse<T>(message: impl Into<String>) -> Result<T, ToolError> {
    Err(ToolError::Refused(message.into()))
}

/// Проверенный вход `remember`/`supersede`.
struct Draft {
    kind: Kind,
    text: String,
    name: Option<String>,
    description: Option<String>,
    tags: Option<String>,
    visibility: Visibility,
    importance: f64,
    occurred_at: Option<Timestamp>,
}

#[allow(clippy::too_many_arguments)]
fn validate<C: Components>(
    ctx: &TurnMemory<'_, C>,
    kind: Kind,
    text: String,
    name: Option<String>,
    description: Option<String>,
    tags: Vec<String>,
    visibility: Option<Visibility>,
    importance: Option<Percent>,
    occurred_at: Option<Timestamp>,
) -> Result<Draft, ToolError> {
    let cfg = &ctx.shared.cfg().memory;
    if matches!(kind, Kind::Core | Kind::Procedure) && !ctx.view.trusted_chat {
        return refuse(TRUSTED_ONLY);
    }
    let text = text.trim().to_owned();
    if text.is_empty() {
        return refuse("text is empty");
    }
    let max = match kind {
        Kind::Procedure => cfg.procedure.body_max_tokens,
        _ => cfg.text_max_tokens,
    };
    let tokens = estimate_bytes(text.len(), ctx.ratio);
    if tokens > max {
        return refuse(format!("text is {tokens} tokens, limit {max}; shorten it"));
    }
    let (name, description) = match kind {
        Kind::Procedure => {
            let name = name
                .map(|n| n.trim().to_lowercase())
                .filter(|n| !n.is_empty())
                .ok_or_else(|| ToolError::Refused(String::from("procedure needs a name")))?;
            if !is_slug(&name, NAME_MAX) {
                return refuse(format!("name must match [a-z0-9_-]{{1,{NAME_MAX}}}"));
            }
            let description = description
                .map(|d| d.trim().to_owned())
                .filter(|d| !d.is_empty())
                .ok_or_else(|| ToolError::Refused(String::from("procedure needs a description")))?;
            let tokens = estimate_bytes(description.len(), ctx.ratio);
            if tokens > cfg.procedure.description_max_tokens {
                return refuse(format!(
                    "description is {tokens} tokens, limit {}",
                    cfg.procedure.description_max_tokens
                ));
            }
            (Some(name), Some(description))
        }
        _ if name.is_some() || description.is_some() => {
            return refuse("name and description belong to procedure only");
        }
        _ => (None, None),
    };
    if occurred_at.is_some() && kind != Kind::Event {
        return refuse("occurred_at belongs to event only");
    }
    let mut slugs: Vec<String> = tags.iter().map(|t| t.trim().to_lowercase()).collect();
    slugs.sort();
    slugs.dedup();
    if slugs.len() > TAGS_MAX {
        return refuse(format!("at most {TAGS_MAX} tags"));
    }
    if let Some(bad) = slugs.iter().find(|t| !is_slug(t, TAG_MAX)) {
        return refuse(format!("tag {bad:?} must match [a-z0-9_-]{{1,{TAG_MAX}}}"));
    }
    Ok(Draft {
        kind,
        text,
        name,
        description,
        tags: (!slugs.is_empty()).then(|| slugs.join(" ")),
        visibility: visibility
            .unwrap_or_else(|| default_visibility(ctx.view.trusted_chat, ctx.view.audience)),
        importance: importance.map_or(0.5, Percent::to_unit),
        occurred_at,
    })
}

fn master<C: Components>(ctx: &TurnMemory<'_, C>) -> Result<(), ToolError> {
    ctx.view
        .master_turn
        .then_some(())
        .ok_or_else(|| ToolError::Refused(String::from(MASTER_ONLY)))
}

/// Эмбеддинг текста ради дедупа и записи, не дольше `write_timeout`; private
/// удалённому эмбеддеру не уходит (`03 §8`).
async fn embed_for_write<C: Components>(
    ctx: &TurnMemory<'_, C>,
    embedder: Option<&Embedder<C>>,
    draft: &Draft,
) -> Option<(ModelId, Vec<f32>)> {
    let embedder = embedder.filter(|e| storable(*e, draft.visibility))?;
    let timeout = *ctx.shared.cfg().memory.embed.write_timeout;
    let vector = embedder.query(&draft.text, timeout).await?;
    Some((embedder.model().clone(), vector))
}

/// Текст строки можно отдать эмбеддеру: локальный или не-private (`03 §8`).
fn storable(embedder: &Embedder<impl Components>, visibility: Visibility) -> bool {
    embedder.local() || visibility != Visibility::Private
}

fn not_found(id: MemoryId) -> ToolError {
    ToolError::Refused(format!("memory {id} not found"))
}

/// Цель `supersede`/`forget`: недоступная туру неотличима от отсутствующей (`03 §7`).
fn current_under(gate: Gate, id: MemoryId, row: Option<MemoryRow>) -> Result<MemoryRow, ToolError> {
    let row = row
        .filter(|r| gate.allows(r.visibility))
        .ok_or_else(|| not_found(id))?;
    if row.valid_to.is_some() {
        return refuse(format!(
            "memory {id} is not current; recall it again for the live version"
        ));
    }
    Ok(row)
}

fn current<C: Components>(
    ctx: &TurnMemory<'_, C>,
    id: MemoryId,
    row: Option<MemoryRow>,
) -> Result<MemoryRow, ToolError> {
    current_under(Gate::turn(ctx.view), id, row)
}

fn name_taken(name: &str, holder: MemoryId) -> String {
    format!("procedure name {name:?} is taken by {holder}; supersede or forget it first")
}

/// Соседи выше `neighbour_floor` по нормированному RRF (FTS ∪ cos), кроме
/// самой записи и строки, с которой её слили.
fn neighbours(
    fts: &[MemoryId],
    cos: &[MemoryId],
    rows: &BTreeMap<MemoryId, &MemoryRow>,
    exclude: &[MemoryId],
    now: Timestamp,
    cfg: &MemoryConfig,
) -> Vec<Neighbour> {
    let fused = rrf(&[fts, cos], cfg.recall.rrf_k);
    let best = fused.values().copied().fold(0.0_f64, f64::max);
    if best <= 0.0 {
        return Vec::new();
    }
    let mut scored: Vec<(MemoryId, f64)> = fused
        .into_iter()
        .filter(|(id, score)| !exclude.contains(id) && score / best >= cfg.dedup.neighbour_floor)
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    scored
        .into_iter()
        .filter_map(|(id, _)| rows.get(&id))
        .take(cfg.dedup.neighbours as usize)
        .map(|row| Neighbour {
            id: row.id,
            kind: row.kind,
            age: text::age(now, row.valid_from),
            text: row.text.clone(),
        })
        .collect()
}

/// Дубль среди current всех kind (`03 §6`): лучший по `max(J/J₀, cos/cos₀)`.
fn dedup_target(
    text: &str,
    rows: &[MemoryRow],
    vector: Option<&[f32]>,
    vectors: &BTreeMap<MemoryId, Vec<f32>>,
    cfg: &MemoryConfig,
) -> Option<MemoryId> {
    let grams = trigrams(text);
    let mut best: Option<(MemoryId, f64)> = None;
    for row in rows {
        let j = jaccard(&grams, &trigrams(&row.text)) / cfg.dedup.jaccard;
        let c = match (vector, vectors.get(&row.id)) {
            (Some(q), Some(v)) => text::cosine(q, v) / cfg.dedup.cos,
            _ => 0.0,
        };
        let score = j.max(c);
        if score >= 1.0 && best.is_none_or(|(_, s)| score > s) {
            best = Some((row.id, score));
        }
    }
    best.map(|(id, _)| id)
}

fn provenance<C: Components>(ctx: &TurnMemory<'_, C>, draft: &Draft, now: Timestamp) -> NewMemory {
    NewMemory {
        kind: draft.kind,
        text: draft.text.clone(),
        name: draft.name.clone(),
        description: draft.description.clone(),
        tags: draft.tags.clone(),
        visibility: draft.visibility,
        importance: draft.importance,
        confidence: 1.0,
        source_actor: ctx.view.actor.clone(),
        source_chat: Some(ctx.view.chat),
        source_surface: ctx.surface.cloned(),
        source_trusted_chat: Some(ctx.view.trusted_chat),
        source_run: ctx.run_id.cloned(),
        occurred_at: draft.occurred_at,
        valid_from: now,
        supersedes: None,
        version: 1,
        last_reinforced_at: now,
        access_count: 0.0,
        now,
    }
}

fn publish<C: Components>(shared: &Shared<C>, ids: Vec<MemoryId>, op: MemoryOp) {
    shared
        .bus()
        .publish(Event::MemoryChanged(MemoryChanged { ids, op }), None);
}

/// `memory_remember` (`03 §6`, `§7`).
#[fastrace::trace]
pub async fn remember<C: Components>(
    ctx: &TurnMemory<'_, C>,
    args: remember::Args,
) -> Result<remember::Ok, ToolError> {
    master(ctx)?;
    let remember::Args {
        kind,
        text: body,
        visibility,
        importance,
        occurred_at,
        valid_from,
        tags,
        name,
        description,
    } = args;
    let draft = validate(
        ctx,
        kind,
        body,
        name,
        description,
        tags,
        visibility,
        importance,
        occurred_at,
    )?;
    let shared = ctx.shared;
    let cfg = &shared.cfg().memory;
    let now = shared.now();

    let embedder = Embedder::resolve(shared);
    let vector = embed_for_write(ctx, embedder.as_ref(), &draft).await;
    let gate = Gate::turn(ctx.view);
    let (text, n) = (draft.text.clone(), cfg.recall.candidates.max(1));
    let query = vector.clone();
    let (visible, vectors, found) = shared
        .db()
        .reader()
        .await?
        .run(move |c| {
            // Слияние с недоступной строкой или её сосед в ответе подтвердили бы её существование.
            let visible: Vec<MemoryRow> = c
                .memory()
                .current()?
                .into_iter()
                .filter(|r| gate.allows(r.visibility))
                .collect();
            let scope = Scope::default().up_to(gate.ceiling());
            let vectors = match &query {
                Some((model, _)) => c.memory().vectors(&scope, model)?,
                None => Vec::new(),
            };
            let query = query.as_ref().map(|(m, v)| (m, v.as_slice()));
            let found = recall_impl::candidates(c, &text, query, &Scope::default(), gate, n)?;
            Ok::<_, DbError>((visible, vectors, found))
        })
        .await?;
    let by_id: BTreeMap<MemoryId, &MemoryRow> = visible.iter().map(|r| (r.id, r)).collect();
    let vectors: BTreeMap<MemoryId, Vec<f32>> = vectors.into_iter().collect();
    let vector = vector.map(|(_, v)| v);

    let target = dedup_target(&draft.text, &visible, vector.as_deref(), &vectors, cfg);
    let (id, action, kind) = match target.and_then(|id| by_id.get(&id).copied()) {
        Some(existing) => {
            merge(
                ctx,
                existing,
                &draft,
                vector.as_deref(),
                embedder.as_ref(),
                now,
            )
            .await?
        }
        None => {
            let new = NewMemory {
                valid_from: valid_from.unwrap_or(now),
                ..provenance(ctx, &draft, now)
            };
            let store = vector
                .clone()
                .zip(embedder.as_ref().map(|e| e.model().clone()));
            let name = draft.name.clone();
            let zone = shared.day_zone();
            let id = shared
                .db()
                .write(move |tx| {
                    super::changes::track(tx, now, &zone, |tx| -> Result<MemoryId, ToolError> {
                        if let Some(name) = &name
                            && let Some(holder) = tx.memory().procedure_named(name, None)?
                        {
                            return refuse(name_taken(name, holder));
                        }
                        let id = tx.memory().insert(&new)?;
                        if let Some((vector, model)) = &store {
                            tx.memory().set_embedding(id, model, vector)?;
                        }
                        Ok(id)
                    })
                })
                .await?;
            shared
                .bump(Metric::MemoryWrites, &draft.kind.to_string())
                .await;
            (id, Action::Inserted, draft.kind)
        }
    };
    let exclude: Vec<MemoryId> = [Some(id), target].into_iter().flatten().collect();
    let neighbours = neighbours(&found.fts, &found.cos, &by_id, &exclude, now, cfg);
    publish(shared, vec![id], MemoryOp::Remember);
    log::info!(memory:% = id, kind:% = kind, action:% = action, actor:? = ctx.view.actor; "memory remembered");
    Ok(remember::Ok {
        id,
        action,
        neighbours,
    })
}

/// Слияние с дублем (`03 §6`): метаданные строже и выше, richer-текст;
/// смена текста — supersede с архивом `merge`, иначе `version + 1` на месте.
async fn merge<C: Components>(
    ctx: &TurnMemory<'_, C>,
    existing: &MemoryRow,
    draft: &Draft,
    vector: Option<&[f32]>,
    embedder: Option<&Embedder<C>>,
    now: Timestamp,
) -> Result<(MemoryId, Action, Kind), ToolError> {
    let shared = ctx.shared;
    let cfg = &shared.cfg().memory;
    let kind = if existing.kind == Kind::Core || draft.kind == Kind::Core {
        Kind::Core
    } else {
        existing.kind
    };
    if matches!(kind, Kind::Core | Kind::Procedure) && !ctx.view.trusted_chat {
        return refuse(TRUSTED_ONLY);
    }
    let visibility = existing.visibility.max(draft.visibility);
    let importance = existing.importance.max(draft.importance);
    let richer_new = estimate_bytes(draft.text.len(), ctx.ratio)
        > estimate_bytes(existing.text.len(), ctx.ratio);
    let tags = match (&existing.tags, &draft.tags) {
        (Some(a), Some(b)) => {
            let mut all: Vec<&str> = a.split_whitespace().chain(b.split_whitespace()).collect();
            all.sort_unstable();
            all.dedup();
            Some(all.join(" "))
        }
        (a, b) => a.clone().or_else(|| b.clone()),
    };
    let meta_changed = kind != existing.kind
        || visibility != existing.visibility
        || importance != existing.importance
        || tags != existing.tags;
    let delta = cfg.reinforce.recall;
    let existing_id = existing.id;

    // Имя procedure — её адрес в каталогах всех чатов: слияние его не меняет.
    let (name, description) = match kind {
        Kind::Procedure => (
            existing.name.clone().or_else(|| draft.name.clone()),
            draft
                .description
                .clone()
                .or_else(|| existing.description.clone()),
        ),
        _ => (None, None),
    };

    let (id, action) = if richer_new {
        let new = NewMemory {
            kind,
            text: draft.text.clone(),
            name,
            description,
            tags,
            visibility,
            importance,
            confidence: existing.confidence,
            occurred_at: draft.occurred_at.or(existing.occurred_at),
            valid_from: now,
            supersedes: Some(existing.id),
            version: existing.version + 1,
            last_reinforced_at: now,
            access_count: existing.access_count + delta,
            ..provenance(ctx, draft, now)
        };
        let store = embedder
            .filter(|e| storable(*e, visibility))
            .zip(vector)
            .map(|(e, v)| (e.model().clone(), v.to_vec()));
        let actor = ctx.view.actor.clone();
        let zone = shared.day_zone();
        let id = shared
            .db()
            .write(move |tx| {
                super::changes::track(tx, now, &zone, |tx| {
                    tx.memory().archive(
                        existing_id,
                        ArchiveReason::Merge,
                        "merged by memory_remember",
                        None,
                        actor.as_ref(),
                        now,
                    )?;
                    let id = tx.memory().insert(&new)?;
                    if let Some((model, vector)) = &store {
                        tx.memory().set_embedding(id, model, vector)?;
                    }
                    Ok::<_, DbError>(id)
                })
            })
            .await?;
        shared.bump(Metric::MemoryWrites, &kind.to_string()).await;
        (id, Action::Merged)
    } else {
        let meta = Meta {
            kind,
            visibility,
            importance,
            version: existing.version + if meta_changed { 1 } else { 0 },
            now,
        };
        let tags_sql = tags.clone();
        // Вектор, посчитанный удалённым эмбеддером, пока строка не была private, ей больше не положен.
        let drop_vector = visibility == Visibility::Private
            && existing.embed_model.is_some()
            && !embedder.is_some_and(Embedder::local);
        let zone = shared.day_zone();
        shared
            .db()
            .write(move |tx| {
                super::changes::track(tx, now, &zone, |tx| {
                    if meta_changed {
                        tx.memory().set_meta(existing_id, &meta)?;
                        tx.memory().set_tags(existing_id, tags_sql.as_deref())?;
                    }
                    if drop_vector {
                        tx.memory().clear_embedding(existing_id)?;
                    }
                    tx.memory().reinforce(&[existing_id], delta, Some(now))
                })
            })
            .await?;
        (
            existing_id,
            if meta_changed {
                Action::Merged
            } else {
                Action::Reinforced
            },
        )
    };
    shared
        .bump(Metric::MemoryDedupMerged, &kind.to_string())
        .await;
    Ok((id, action, kind))
}

/// `memory_supersede` (`03 §7`): новая версия, старая закрыта без архива.
#[fastrace::trace]
pub async fn supersede<C: Components>(
    ctx: &TurnMemory<'_, C>,
    args: supersede::Args,
) -> Result<supersede::Ok, ToolError> {
    master(ctx)?;
    let supersede::Args {
        id,
        text: body,
        kind,
        visibility,
        importance,
        occurred_at,
        valid_from,
        tags,
        name,
        description,
    } = args;
    let shared = ctx.shared;
    let old = shared
        .db()
        .reader()
        .await?
        .run(move |c| c.memory().get(id))
        .await?;
    let old = current(ctx, id, old)?;
    if matches!(old.kind, Kind::Core | Kind::Procedure) && !ctx.view.trusted_chat {
        return refuse(TRUSTED_ONLY);
    }
    let kind = kind.unwrap_or(old.kind);
    let (name, description) = match kind {
        Kind::Procedure => (
            name.or_else(|| old.name.clone()),
            description.or_else(|| old.description.clone()),
        ),
        _ => (name, description),
    };
    let tags = tags.unwrap_or_else(|| {
        old.tags
            .as_deref()
            .map(|t| t.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default()
    });
    let draft = validate(
        ctx,
        kind,
        body,
        name,
        description,
        tags,
        Some(visibility.unwrap_or(old.visibility)),
        Some(importance.unwrap_or_else(|| Percent::from_unit(old.importance))),
        occurred_at
            .or(old.occurred_at)
            .filter(|_| kind == Kind::Event),
    )?;
    let now = shared.now();
    let embedder = Embedder::resolve(shared);
    let store = embed_for_write(ctx, embedder.as_ref(), &draft).await;
    let new = NewMemory {
        confidence: old.confidence,
        valid_from: valid_from.unwrap_or(now),
        supersedes: Some(old.id),
        version: old.version + 1,
        access_count: old.access_count,
        ..provenance(ctx, &draft, now)
    };
    let old_id = old.id;
    let name = draft.name.clone();
    let zone = shared.day_zone();
    let new_id = shared
        .db()
        .write(move |tx| {
            super::changes::track(tx, now, &zone, |tx| -> Result<MemoryId, ToolError> {
                if let Some(name) = &name
                    && let Some(holder) = tx.memory().procedure_named(name, Some(old_id))?
                {
                    return refuse(name_taken(name, holder));
                }
                tx.memory().close(old_id, now)?;
                let id = tx.memory().insert(&new)?;
                if let Some((model, vector)) = &store {
                    tx.memory().set_embedding(id, model, vector)?;
                }
                Ok(id)
            })
        })
        .await?;
    shared
        .bump(Metric::MemoryWrites, &draft.kind.to_string())
        .await;
    publish(shared, vec![new_id, old_id], MemoryOp::Supersede);
    log::info!(memory:% = new_id, supersedes:% = old_id, kind:% = draft.kind; "memory superseded");
    Ok(supersede::Ok {
        id: new_id,
        supersedes: old_id,
        version: old.version + 1,
    })
}

/// `memory_forget` (`03 §7`, `§9`): `valid_to = now` + архив `forget`; `run_id` всегда NULL.
#[fastrace::trace]
pub async fn forget<C: Components>(
    ctx: &TurnMemory<'_, C>,
    args: forget::Args,
) -> Result<forget::Ok, ToolError> {
    master(ctx)?;
    let forget::Args { id, why } = args;
    let why = why.trim().to_owned();
    if why.is_empty() {
        return refuse("why is required");
    }
    let shared = ctx.shared;
    let now = shared.now();
    let actor = ctx.view.actor.clone();
    let gate = Gate::turn(ctx.view);
    let zone = shared.day_zone();
    let kind = shared
        .db()
        .write(move |tx| {
            super::changes::track(tx, now, &zone, |tx| -> Result<Kind, ToolError> {
                let row = tx.memory().get(id)?;
                let row = current_under(gate, id, row)?;
                tx.memory()
                    .archive(id, ArchiveReason::Forget, &why, None, actor.as_ref(), now)?;
                Ok(row.kind)
            })
        })
        .await?;
    shared
        .bump(Metric::MemoryForgotten, &kind.to_string())
        .await;
    publish(shared, vec![id], MemoryOp::Forget);
    log::info!(memory:% = id, kind:% = kind; "memory forgotten");
    Ok(forget::Ok {
        id,
        archived_until: now + *shared.cfg().gc.memory_archive_after,
    })
}

/// `memory_recall` (`03 §4`, `§7`): видимость по туру, hot-блок исключён.
#[fastrace::trace]
pub async fn recall<C: Components>(
    ctx: &TurnMemory<'_, C>,
    args: recall::Args,
) -> Result<recall::Ok, ToolError> {
    let recall::Args {
        query,
        kind,
        at,
        since,
        until,
        limit,
    } = args;
    if query.trim().is_empty() {
        return refuse("query is empty");
    }
    let now = ctx.shared.now();
    let recalled = recall_impl::recall(
        ctx.shared,
        Gate::turn(ctx.view),
        ctx.hot_ids,
        recall_impl::Query {
            text: query,
            kind,
            at,
            since,
            until,
            limit,
        },
    )
    .await?;
    Ok(recall::Ok {
        hits: recalled
            .hits
            .into_iter()
            .map(|h| recall::Hit {
                id: h.row.id,
                kind: h.row.kind,
                age: text::age(now, h.row.valid_from),
                importance: Percent::from_unit(h.row.importance),
                visibility: h.row.visibility,
                text: h.row.text,
            })
            .collect(),
        withheld: recalled.withheld,
    })
}

/// `memory_read`: полная строка с сильным подкреплением; недоступная — not found.
#[fastrace::trace]
pub async fn read<C: Components>(
    ctx: &TurnMemory<'_, C>,
    args: read::Args,
) -> Result<(Record, Option<Archive>), ToolError> {
    let read::Args { id } = args;
    let gate = Gate::turn(ctx.view);
    let shared = ctx.shared;
    let row = shared
        .db()
        .reader()
        .await?
        .run(move |c| c.memory().get(id))
        .await?
        .filter(|row| gate.allows(row.visibility))
        .ok_or_else(|| not_found(id))?;
    let (now, delta) = (shared.now(), shared.cfg().memory.reinforce.recall);
    shared
        .db()
        .write(move |tx| tx.memory().reinforce(&[id], delta, Some(now)))
        .await?;
    Ok(convert::memory::record(row))
}

/// `memory_history`: цепочка версий, прореженная видимостью; запрошенная
/// версия решает, есть ли ответ.
#[fastrace::trace]
pub async fn history<C: Components>(
    ctx: &TurnMemory<'_, C>,
    args: read::Args,
) -> Result<Vec<Version>, ToolError> {
    let read::Args { id } = args;
    let gate = Gate::turn(ctx.view);
    let chain = ctx
        .shared
        .db()
        .reader()
        .await?
        .run(move |c| c.memory().chain(id))
        .await?;
    if !chain
        .iter()
        .any(|row| row.id == id && gate.allows(row.visibility))
    {
        return Err(not_found(id));
    }
    Ok(chain
        .into_iter()
        .filter(|row| gate.allows(row.visibility))
        .map(convert::memory::version)
        .collect())
}

/// Строка записи в тексте результата: как в hot-блоке, одна запись = одна строка.
fn hit_line(
    id: MemoryId,
    kind: Kind,
    age: &str,
    importance: Option<Percent>,
    visibility: Option<Visibility>,
    body: &str,
) -> String {
    let mut parts = vec![format!("- {id}"), kind.to_string(), age.to_owned()];
    if let Some(importance) = importance {
        parts.push(format!("i{:.1}", importance.to_unit()));
    }
    match visibility {
        Some(Visibility::Trusted) => parts.push(String::from("trusted")),
        Some(Visibility::Private) => parts.push(String::from("private")),
        Some(Visibility::Public) | None => {}
    }
    parts.push(sanitize_inline(body));
    parts.join(" · ")
}

/// Результат `memory_recall` для модели: пусто — `NOTHING_NEW`, отсечённое — маркер без счётчика.
pub fn recall_text(ok: &recall::Ok) -> String {
    let mut lines: Vec<String> = ok
        .hits
        .iter()
        .map(|h| {
            hit_line(
                h.id,
                h.kind,
                &h.age,
                Some(h.importance),
                Some(h.visibility),
                &h.text,
            )
        })
        .collect();
    if lines.is_empty() {
        lines.push(String::from(NOTHING_NEW));
    }
    if ok.withheld {
        lines.push(String::from(WITHHELD));
    }
    lines.join("\n")
}

/// Результат `memory_remember`: действие и соседи с пометкой `RECONCILE`.
pub fn remember_text(ok: &remember::Ok) -> String {
    let mut out = format!("{} {}", ok.action, ok.id);
    if !ok.neighbours.is_empty() {
        out.push_str(&format!("\nneighbours — {RECONCILE}:"));
        for n in &ok.neighbours {
            out.push('\n');
            out.push_str(&hit_line(n.id, n.kind, &n.age, None, None, &n.text));
        }
    }
    out
}

pub fn supersede_text(ok: &supersede::Ok) -> String {
    format!(
        "{} supersedes {} (version {})",
        ok.id, ok.supersedes, ok.version
    )
}

pub fn forget_text(ok: &forget::Ok, now: Timestamp) -> String {
    format!(
        "forgotten {}; the owner can restore it for {}",
        ok.id,
        text::age(ok.archived_until, now)
    )
}
