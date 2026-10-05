//! Recall (`03 §4`): FTS5 ∪ cos → RRF · salience, относительный floor,
//! исключение hot-блока, сильное подкрепление возвращённых.

use std::collections::{BTreeMap, BTreeSet};

use eva::time::Timestamp;
use eva_protocol::{
    errors::AuxResult,
    types::{MemoryId, ModelId, memory::Kind},
};

use crate::{
    components::Components,
    config::{MemoryConfig, memory::RecallConfig},
    db::{
        Connection, Db as _, DbError, DbResult, Queries as _,
        memory::{MemoryRow, Scope},
        stats::Metric,
    },
    shared::Shared,
    subsystems::memory::{
        embed::Embedder,
        salience,
        text::{cosine, fts_query, rrf},
        visibility::Gate,
    },
};

/// Пустой результат: гасит повторные вызовы по кругу.
pub const NOTHING_NEW: &str =
    "nothing new beyond the memory block; only a different topic is worth another call";

#[derive(Debug, Clone, Default)]
pub struct Query {
    pub text: String,
    pub kind: Option<Kind>,
    /// Версии, истинные в этот момент; `None` — текущие.
    pub at: Option<Timestamp>,
    pub since: Option<Timestamp>,
    pub until: Option<Timestamp>,
    /// `None` — `memory.recall.limit`; потолок `RecallConfig::LIMIT_MAX`.
    pub limit: Option<u8>,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub row: MemoryRow,
    pub score: f64,
}

#[derive(Debug, Clone, Default)]
pub struct Recalled {
    pub hits: Vec<Hit>,
    /// Фильтр видимости что-то отсёк.
    pub withheld: bool,
}

/// `score = rrf · salience`, `score < best · floor` отброшены; стабильный
/// порядок по убыванию и `id`.
pub fn rank(
    fts: &[MemoryId],
    cos: &[MemoryId],
    rows: &BTreeMap<MemoryId, MemoryRow>,
    now: Timestamp,
    cfg: &MemoryConfig,
) -> Vec<(MemoryId, f64)> {
    let fused = rrf(&[fts, cos], cfg.recall.rrf_k);
    let mut scored: Vec<(MemoryId, f64)> = fused
        .into_iter()
        .filter_map(|(id, r)| {
            rows.get(&id)
                .map(|row| (id, r * salience::of_row(row, now, &cfg.salience)))
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let best = scored.first().map_or(0.0, |s| s.1);
    scored.retain(|(_, s)| *s >= best * cfg.recall.floor);
    scored
}

/// Top-`n` по cos к запросу, убывание.
pub fn by_cosine(query: &[f32], vectors: &[(MemoryId, Vec<f32>)], n: usize) -> Vec<MemoryId> {
    let mut scored: Vec<(MemoryId, f64)> = vectors
        .iter()
        .map(|(id, v)| (*id, cosine(query, v)))
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.into_iter().take(n).map(|(id, _)| id).collect()
}

/// Кандидаты FTS и cos под видимостью тура (`03 §4` п.1–3).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Candidates {
    pub fts: Vec<MemoryId>,
    pub cos: Vec<MemoryId>,
    /// Недоступная строка попала бы в кандидаты: её FTS-матч или место в top cos без фильтра.
    pub withheld: bool,
}

/// Фильтр видимости стоит в `WHERE` обоих источников, чтобы недоступные строки
/// не вытесняли доступные из top; `vector` — эмбеддинг запроса под моделью.
pub fn candidates(
    conn: &Connection,
    text: &str,
    vector: Option<(&ModelId, &[f32])>,
    scope: &Scope,
    gate: Gate,
    n: u32,
) -> DbResult<Candidates> {
    let shown = scope.clone().up_to(gate.ceiling());
    let hidden = shown.hidden();
    let q = fts_query(text, 3);
    let fts: Vec<MemoryId> = conn
        .memory()
        .fts(&q, &shown, n)?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let mut withheld = match &hidden {
        Some(hidden) => !conn.memory().fts(&q, hidden, 1)?.is_empty(),
        None => false,
    };
    let cos = match vector {
        Some((model, query)) => {
            let visible = conn.memory().vectors(&shown, model)?;
            let top = by_cosine(query, &visible, n as usize);
            if let (false, Some(hidden)) = (withheld, &hidden) {
                let concealed = conn.memory().vectors(hidden, model)?;
                let concealed_ids: BTreeSet<MemoryId> =
                    concealed.iter().map(|(id, _)| *id).collect();
                let all: Vec<(MemoryId, Vec<f32>)> = visible.into_iter().chain(concealed).collect();
                withheld = by_cosine(query, &all, n as usize)
                    .iter()
                    .any(|id| concealed_ids.contains(id));
            }
            top
        }
        None => Vec::new(),
    };
    Ok(Candidates { fts, cos, withheld })
}

/// Явный recall: `gate` — видимость тура, `hot_ids` — строки, уже стоящие в
/// hot-блоке сегмента.
pub async fn recall<C: Components>(
    shared: &Shared<C>,
    gate: Gate,
    hot_ids: &[MemoryId],
    query: Query,
) -> AuxResult<Recalled> {
    let cfg = &shared.cfg().memory;
    let now = shared.now();
    let scope = Scope {
        at: query.at,
        kind: query.kind,
        since: query.since,
        until: query.until,
        ..Scope::default()
    };
    let n = cfg.recall.candidates.max(1);
    let embedder = Embedder::resolve(shared);
    let vector = match &embedder {
        Some(e) => e
            .query(&query.text, *cfg.embed.query_timeout)
            .await
            .map(|v| (e.model().clone(), v)),
        None => None,
    };
    let embedded = vector.is_some();

    let reader = shared.db().reader().await?;
    let text = query.text.clone();
    let found = reader
        .run(move |c| {
            let found = candidates(
                c,
                &text,
                vector.as_ref().map(|(m, v)| (m, v.as_slice())),
                &scope,
                gate,
                n,
            )?;
            let mut union: Vec<MemoryId> = found.fts.iter().chain(&found.cos).copied().collect();
            union.sort_unstable();
            union.dedup();
            let rows = c.memory().rows(&union)?;
            Ok::<_, DbError>((found, rows))
        })
        .await?;
    let (Candidates { fts, cos, withheld }, rows) = found;
    let mut rows: BTreeMap<MemoryId, MemoryRow> = rows.into_iter().map(|r| (r.id, r)).collect();

    let limit = usize::from(
        query
            .limit
            .map_or(cfg.recall.limit, |l| l.min(RecallConfig::LIMIT_MAX))
            .max(1),
    );
    let hits: Vec<Hit> = rank(&fts, &cos, &rows, now, cfg)
        .into_iter()
        .filter(|(id, _)| !hot_ids.contains(id))
        .take(limit)
        .filter_map(|(id, score)| rows.remove(&id).map(|row| Hit { row, score }))
        .collect();

    if hits.is_empty() {
        shared.bump(Metric::MemoryRecallEmpty, "").await;
    } else {
        let ids: Vec<MemoryId> = hits.iter().map(|h| h.row.id).collect();
        let delta = cfg.reinforce.recall;
        shared
            .db()
            .write(move |tx| tx.memory().reinforce(&ids, delta, Some(now)))
            .await?;
        shared.bump(Metric::MemoryRecallHits, "").await;
    }
    if withheld {
        shared.bump(Metric::MemoryRecallWithheld, "").await;
    }
    log::debug!(hits = hits.len(), withheld, embedded; "memory recalled");
    Ok(Recalled { hits, withheld })
}
