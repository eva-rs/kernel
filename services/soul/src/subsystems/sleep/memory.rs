//! Фаза 1 сна (`08 §2.3`): снапшот корпуса, кластеры, forget-кандидаты,
//! промпт, vet операций тулов `sleep_memory_*` и их применение.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use eva::{data, time::Timestamp};
use serde_json::{Value, json};

use eva_protocol::types::{
    MemoryId, RunId,
    memory::{ArchiveReason, Kind, Percent, Visibility},
    sleep::{PhaseOp, Verdict},
};

use crate::{
    components::Components,
    config::{MemoryConfig, memory::HalfLife, sleep::SleepConfig},
    db::{
        Db as _, DbError, DbResult, Queries as _, Tx,
        memory::{MemoryRow, NewMemory, Scope},
        sleep::Merged,
        stats::{Metric, ago, elapsed},
    },
    markers::sanitize_inline,
    shared::Shared,
};

/// Каноны тулов фазы.
pub const TOOLS: [&str; 6] = [
    "sleep_memory_forget",
    "sleep_memory_merge",
    "sleep_memory_split",
    "sleep_memory_change_type",
    "sleep_memory_supersede",
    "sleep_memory_demote",
];

/// Причины отказа vet (`08 §2.3`): текст в `is_error` и в `ops_json`.
pub const WHY_REQUIRED: &str = "why required";
pub const ID_NOT_SHOWN: &str = "id not shown";
pub const STALE: &str = "stale: changed since snapshot";
pub const CORE_NEVER_DELETED: &str = "core is never deleted, demote it";
pub const NOT_CORE: &str = "not core";
pub const TOO_RECENT: &str = "too recent";
pub const NOT_CANDIDATE: &str = "not a forget candidate";
pub const UNTRUSTED: &str = "untrusted source";
pub const CORE_OVER_BUDGET: &str = "core over budget";
pub const CAP_REACHED: &str = "cap reached";

/// Без калибровки `token_ratios` — четыре байта на токен.
const DEFAULT_BYTES_PER_TOKEN: f64 = 4.0;
/// Длина текста записи в промпте.
const LINE_CHARS: usize = 240;

/// Операция модели: вход тула, разобранный по схеме; тег — канон без `sleep_memory_`.
#[data]
pub enum Op {
    Forget {
        id: MemoryId,
        why: String,
    },
    Merge {
        ids: Vec<MemoryId>,
        text: String,
        #[serde(default)]
        kind: Option<Kind>,
        #[serde(default)]
        importance: Option<Percent>,
        why: String,
    },
    Split {
        id: MemoryId,
        into: Vec<Shard>,
        why: String,
    },
    ChangeType {
        id: MemoryId,
        kind: Kind,
        why: String,
    },
    Supersede {
        id: MemoryId,
        text: String,
        why: String,
    },
    Demote {
        id: MemoryId,
        why: String,
    },
}

#[data]
pub struct Shard {
    pub text: String,
    #[serde(default)]
    pub kind: Option<Kind>,
    #[serde(default)]
    pub importance: Option<Percent>,
}

impl Op {
    /// Вход тула по канону; ошибка — имя поля или форма.
    pub fn parse(tool: &str, input: &Value) -> Result<Self, String> {
        let tag = tool
            .strip_prefix("sleep_memory_")
            .filter(|_| TOOLS.contains(&tool))
            .ok_or_else(|| format!("unknown sleep tool {tool}"))?;
        let op: Self = serde_json::from_value(json!({ tag: input })).map_err(|e| e.to_string())?;
        match &op {
            Self::Merge { ids, .. } if ids.len() < 2 => Err(String::from("ids: at least two")),
            Self::Split { into, .. } if into.len() < 2 => {
                Err(String::from("into: at least two parts"))
            }
            Self::Merge { text, .. } | Self::Supersede { text, .. } if text.trim().is_empty() => {
                Err(String::from("text required"))
            }
            Self::Split { into, .. } if into.iter().any(|s| s.text.trim().is_empty()) => {
                Err(String::from("into[].text required"))
            }
            _ => Ok(op),
        }
    }

    pub fn ids(&self) -> Vec<MemoryId> {
        match self {
            Self::Merge { ids, .. } => ids.clone(),
            Self::Forget { id, .. }
            | Self::Split { id, .. }
            | Self::ChangeType { id, .. }
            | Self::Supersede { id, .. }
            | Self::Demote { id, .. } => vec![*id],
        }
    }

    pub fn why(&self) -> &str {
        match self {
            Self::Forget { why, .. }
            | Self::Merge { why, .. }
            | Self::Split { why, .. }
            | Self::ChangeType { why, .. }
            | Self::Supersede { why, .. }
            | Self::Demote { why, .. } => why,
        }
    }

    /// Закрывает версию источника: правило 5 (`too recent`).
    pub const fn archives(&self) -> bool {
        matches!(
            self,
            Self::Forget { .. } | Self::Merge { .. } | Self::Split { .. } | Self::Supersede { .. }
        )
    }

    /// Виды результата для правила 8; у merge без `kind` — все виды источников.
    fn result_kinds(&self, sources: &[Shown]) -> Vec<Kind> {
        match self {
            Self::Forget { .. } => Vec::new(),
            Self::Demote { .. } => vec![Kind::Fact],
            Self::ChangeType { kind, .. } => vec![*kind],
            Self::Merge {
                kind: Some(kind), ..
            } => vec![*kind],
            Self::Merge { kind: None, .. } | Self::Supersede { .. } => {
                sources.iter().map(|s| s.kind).collect()
            }
            Self::Split { into, .. } => into
                .iter()
                .map(|s| s.kind.unwrap_or(sources[0].kind))
                .collect(),
        }
    }

    /// Токены core после операции: источники core невозможны (правило 4).
    fn core_tokens_after(&self, snap: &Snapshot, sources: &[Shown]) -> u32 {
        let added = match self {
            Self::ChangeType {
                kind: Kind::Core, ..
            } => sources[0].tokens,
            Self::Merge {
                kind: Some(Kind::Core),
                text,
                ..
            } => snap.tokens(text),
            Self::Split { into, .. } => into
                .iter()
                .filter(|s| s.kind.unwrap_or(sources[0].kind) == Kind::Core)
                .map(|s| snap.tokens(&s.text))
                .sum(),
            _ => 0,
        };
        snap.core_tokens.saturating_add(added)
    }
}

/// Снимок записи для vet: то, что решает правила без БД.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shown {
    pub version: u32,
    pub kind: Kind,
    pub visibility: Visibility,
    pub created_at: Timestamp,
    pub trusted: bool,
    pub tokens: u32,
}

/// Снапшот фазы `S` и показанное модели.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub rows: BTreeMap<MemoryId, Shown>,
    pub shown: BTreeSet<MemoryId>,
    pub candidates: BTreeSet<MemoryId>,
    pub clusters: Vec<Vec<MemoryId>>,
    /// Кластеров сверх `sleep.max_clusters`: следующей ночью.
    pub deferred: usize,
    pub core_tokens: u32,
    pub core_budget: u32,
    /// Core-записей, не влезших в бюджет hot-блока.
    pub core_dropped: u32,
    /// Созданное позже — `too recent`.
    pub recent_after: Timestamp,
    pub bytes_per_token: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub applied: u32,
    pub forgets: u32,
}

impl Snapshot {
    pub fn build(
        rows: &[MemoryRow],
        vectors: &HashMap<MemoryId, Vec<f32>>,
        now: Timestamp,
        cfg: &SleepConfig,
        memory: &MemoryConfig,
        bytes_per_token: f64,
    ) -> Self {
        let bytes_per_token = if bytes_per_token > 0.0 {
            bytes_per_token
        } else {
            DEFAULT_BYTES_PER_TOKEN
        };
        let mut snap = Self {
            rows: BTreeMap::new(),
            shown: BTreeSet::new(),
            candidates: BTreeSet::new(),
            clusters: Vec::new(),
            deferred: 0,
            core_tokens: 0,
            core_budget: 0,
            core_dropped: 0,
            recent_after: ago(now, *cfg.keep_recent),
            bytes_per_token,
        };
        for row in rows {
            snap.rows.insert(row.id, snap.shown_of(row));
        }
        snap.core_budget = (f64::from(memory.budget_tokens) * memory.hot.core_share).floor() as u32;
        snap.recount_core();
        let mut core: Vec<&MemoryRow> = rows.iter().filter(|r| r.kind == Kind::Core).collect();
        core.sort_by(|a, b| b.importance.total_cmp(&a.importance).then(a.id.cmp(&b.id)));
        let mut used = 0u32;
        for row in core {
            let tokens = snap.tokens(&row.text);
            if used.saturating_add(tokens) <= snap.core_budget {
                used += tokens;
            } else {
                snap.core_dropped += 1;
            }
        }

        let mut clusters = clusters(rows, vectors, cfg);
        clusters.retain(|members| {
            members
                .iter()
                .any(|id| rows.iter().find(|r| r.id == *id).is_some_and(needs_review))
        });
        let oldest = |members: &[MemoryId]| {
            members
                .iter()
                .filter_map(|id| rows.iter().find(|r| r.id == *id))
                .map(|r| r.updated_at)
                .min()
        };
        clusters.sort_by(|a, b| oldest(a).cmp(&oldest(b)).then(a[0].cmp(&b[0])));
        let shown_clusters = usize::try_from(cfg.max_clusters).unwrap_or(usize::MAX);
        snap.deferred = clusters.len().saturating_sub(shown_clusters);
        clusters.truncate(shown_clusters);
        let in_cluster: BTreeSet<MemoryId> = clusters.iter().flatten().copied().collect();

        let mut candidates: Vec<&MemoryRow> = rows
            .iter()
            .filter(|r| {
                !in_cluster.contains(&r.id)
                    && forget_candidate(r, now, cfg, &memory.salience.half_life)
            })
            .collect();
        candidates.sort_by(|a, b| {
            a.last_reinforced_at
                .cmp(&b.last_reinforced_at)
                .then(a.id.cmp(&b.id))
        });
        candidates.truncate(usize::try_from(2 * cfg.max_forget).unwrap_or(usize::MAX));

        snap.shown = in_cluster;
        snap.candidates = candidates.iter().map(|r| r.id).collect();
        snap.shown.extend(snap.candidates.iter().copied());
        snap.clusters = clusters;
        snap
    }

    pub fn tokens(&self, text: &str) -> u32 {
        (text.len() as f64 / self.bytes_per_token).ceil() as u32
    }

    fn shown_of(&self, row: &MemoryRow) -> Shown {
        Shown {
            version: row.version,
            kind: row.kind,
            visibility: row.visibility,
            created_at: row.created_at,
            trusted: row.source_trusted_chat == Some(true),
            tokens: self.tokens(&row.text),
        }
    }

    fn recount_core(&mut self) {
        self.core_tokens = self
            .rows
            .values()
            .filter(|s| s.kind == Kind::Core)
            .map(|s| s.tokens)
            .fold(0u32, u32::saturating_add);
    }

    /// Результат применённой операции: версии, виды, закрытые записи.
    fn absorb(&mut self, applied: &Applied) {
        for id in &applied.archived {
            self.rows.remove(id);
            self.shown.remove(id);
            self.candidates.remove(id);
        }
        for (id, shown) in &applied.updated {
            self.rows.insert(*id, *shown);
        }
        self.recount_core();
    }
}

/// `consolidated_at IS NULL ∨ updated_at > consolidated_at`.
pub fn needs_review(row: &MemoryRow) -> bool {
    row.consolidated_at.is_none_or(|c| row.updated_at > c)
}

/// Кандидат на забывание (`03 §3`, `08 §2.3`): пороги сна, полураспады salience.
pub fn forget_candidate(
    row: &MemoryRow,
    now: Timestamp,
    cfg: &SleepConfig,
    half_life: &HalfLife,
) -> bool {
    let half_life = match row.kind {
        Kind::Core => return false,
        Kind::Fact => *half_life.fact,
        Kind::Event => *half_life.event,
        Kind::Procedure => *half_life.procedure,
    };
    row.importance <= cfg.forget.importance_max
        && row.access_count < 1.0
        && elapsed(row.last_reinforced_at, now) > half_life.saturating_mul(2)
        && row.created_at < ago(now, *cfg.keep_recent)
}

/// Правила 1–9 по порядку (`08 §2.3`); `live` — текущие версии записей
/// операции (правило 3, транзакция применения сверяет их ещё раз), правило 7 —
/// нормализация, не отказ.
pub fn vet(
    op: &Op,
    snap: &Snapshot,
    live: &BTreeMap<MemoryId, u32>,
    counters: &Counters,
    cfg: &SleepConfig,
) -> Result<(), &'static str> {
    if op.why().trim().is_empty() {
        return Err(WHY_REQUIRED);
    }
    let sources: Vec<Shown> = op
        .ids()
        .iter()
        .map(|id| {
            snap.shown
                .contains(id)
                .then(|| snap.rows.get(id).copied())
                .flatten()
        })
        .collect::<Option<_>>()
        .ok_or(ID_NOT_SHOWN)?;
    if op
        .ids()
        .iter()
        .zip(&sources)
        .any(|(id, s)| live.get(id) != Some(&s.version))
    {
        return Err(STALE);
    }
    let core = sources.iter().any(|s| s.kind == Kind::Core);
    match op {
        Op::Demote { .. } if !core => return Err(NOT_CORE),
        Op::Demote { .. } => {}
        _ if core => return Err(CORE_NEVER_DELETED),
        _ => {}
    }
    if op.archives() && sources.iter().any(|s| s.created_at > snap.recent_after) {
        return Err(TOO_RECENT);
    }
    if let Op::Forget { id, .. } = op
        && !snap.candidates.contains(id)
    {
        return Err(NOT_CANDIDATE);
    }
    let results = op.result_kinds(&sources);
    if results
        .iter()
        .any(|k| matches!(k, Kind::Core | Kind::Procedure))
        && !sources.iter().all(|s| s.trusted)
    {
        return Err(UNTRUSTED);
    }
    if results.contains(&Kind::Core) && op.core_tokens_after(snap, &sources) > snap.core_budget {
        return Err(CORE_OVER_BUDGET);
    }
    let forget = matches!(op, Op::Forget { .. });
    if counters.applied >= cfg.max_ops || (forget && counters.forgets >= cfg.max_forget) {
        return Err(CAP_REACHED);
    }
    Ok(())
}

/// Промпт фазы: байт-стабильный английский рендер (`08 §2.3`, I13).
pub fn render(rows: &[MemoryRow], snap: &Snapshot, now: Timestamp) -> String {
    let by_id: BTreeMap<MemoryId, &MemoryRow> = rows.iter().map(|r| (r.id, r)).collect();
    let count = |kind: Kind| rows.iter().filter(|r| r.kind == kind).count();
    let bucket = |lo: f64, hi: f64| {
        rows.iter()
            .filter(|r| r.importance > lo && r.importance <= hi)
            .count()
    };
    let mut out = String::from("[sleep memory phase]\n");
    out.push_str(&format!(
        "corpus: {} current (fact {}, event {}, core {}, procedure {}); importance <=0.2 {}, <=0.4 {}, <=0.6 {}, <=0.8 {}, >0.8 {}; core {}/{} tokens, {} dropped\n",
        rows.len(),
        count(Kind::Fact),
        count(Kind::Event),
        count(Kind::Core),
        count(Kind::Procedure),
        bucket(-1.0, 0.2),
        bucket(0.2, 0.4),
        bucket(0.4, 0.6),
        bucket(0.6, 0.8),
        bucket(0.8, 2.0),
        snap.core_tokens,
        snap.core_budget,
        snap.core_dropped,
    ));
    out.push_str(&format!(
        "clusters: {} shown, {} deferred\n",
        snap.clusters.len(),
        snap.deferred
    ));
    for (n, members) in snap.clusters.iter().enumerate() {
        out.push_str(&format!("cluster {}:\n", n + 1));
        for id in members {
            if let Some(row) = by_id.get(id) {
                out.push_str(&line(row, now));
            }
        }
    }
    out.push_str(&format!("forget candidates: {}\n", snap.candidates.len()));
    for id in &snap.candidates {
        if let Some(row) = by_id.get(id) {
            out.push_str(&line(row, now));
        }
    }
    out.push_str("tools: forget only ids listed as forget candidates; merge, split, supersede, change_type and demote only ids shown above; every call needs why; finish with sleep_done.");
    out
}

fn line(row: &MemoryRow, now: Timestamp) -> String {
    let recalled = if row.access_count <= 0.0 {
        String::from("never")
    } else {
        let n = if row.access_count.fract() == 0.0 {
            format!("{:.0}", row.access_count)
        } else {
            format!("{:.1}", row.access_count)
        };
        format!("recalled {n}x, last {}", age(row.last_reinforced_at, now))
    };
    let mut text = sanitize_inline(&row.text);
    if text.chars().count() > LINE_CHARS {
        text = text.chars().take(LINE_CHARS).collect::<String>() + "…";
    }
    format!(
        "  {} | {} | {} | {} | i{:.2} c{:.2} | {recalled} | {text}\n",
        row.id,
        row.kind,
        row.visibility,
        age(row.created_at, now),
        row.importance,
        row.confidence,
    )
}

/// `Nd`, `Nh` или `Nm`: возраст в промпте.
pub fn age(from: Timestamp, now: Timestamp) -> String {
    let secs = elapsed(from, now).as_secs();
    match (secs / 86_400, secs / 3_600, secs / 60) {
        (d, _, _) if d >= 1 => format!("{d}d"),
        (_, h, _) if h >= 1 => format!("{h}h"),
        (_, _, m) => format!("{m}m"),
    }
}

/// Триграммы текста хэшами, отсортированные без повторов (`03 §6`).
pub fn trigrams(text: &str) -> Vec<u32> {
    let chars: Vec<char> = text.to_lowercase().chars().collect();
    let mut out: Vec<u32> = if chars.len() < 3 {
        vec![
            chars
                .iter()
                .fold(7u32, |h, c| h.wrapping_mul(31).wrapping_add(*c as u32)),
        ]
    } else {
        chars
            .windows(3)
            .map(|w| {
                w.iter()
                    .fold(7u32, |h, c| h.wrapping_mul(31).wrapping_add(*c as u32))
            })
            .collect()
    };
    out.sort_unstable();
    out.dedup();
    out
}

pub fn jaccard(a: &[u32], b: &[u32]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let (mut i, mut j, mut common) = (0, 0, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
        }
    }
    common as f64 / (a.len() + b.len() - common) as f64
}

pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0f64, 0f64, 0f64);
    for (x, y) in a.iter().zip(b) {
        dot += f64::from(*x) * f64::from(*y);
        na += f64::from(*x) * f64::from(*x);
        nb += f64::from(*y) * f64::from(*y);
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// Кластеры-кандидаты: рёбра по порогам, union-find по убыванию веса,
/// размер ≤ `sleep.cluster.max`; только кластеры из двух и более записей.
pub fn clusters(
    rows: &[MemoryRow],
    vectors: &HashMap<MemoryId, Vec<f32>>,
    cfg: &SleepConfig,
) -> Vec<Vec<MemoryId>> {
    let grams: Vec<Vec<u32>> = rows.iter().map(|r| trigrams(&r.text)).collect();
    let mut edges: Vec<(f64, usize, usize)> = Vec::new();
    for i in 0..rows.len() {
        for j in i + 1..rows.len() {
            let jac = jaccard(&grams[i], &grams[j]);
            let cos = match (vectors.get(&rows[i].id), vectors.get(&rows[j].id)) {
                (Some(a), Some(b)) => cosine(a, b),
                _ => 0.0,
            };
            if jac >= cfg.cluster.jaccard || cos >= cfg.cluster.cosine {
                edges.push((jac.max(cos), i, j));
            }
        }
    }
    edges.sort_by(|a, b| b.0.total_cmp(&a.0).then((a.1, a.2).cmp(&(b.1, b.2))));

    let mut parent: Vec<usize> = (0..rows.len()).collect();
    let mut size = vec![1usize; rows.len()];
    fn root(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    let max = usize::try_from(cfg.cluster.max)
        .unwrap_or(usize::MAX)
        .max(2);
    for (_, i, j) in edges {
        let (a, b) = (root(&mut parent, i), root(&mut parent, j));
        if a != b && size[a] + size[b] <= max {
            parent[b] = a;
            size[a] += size[b];
        }
    }
    let mut groups: BTreeMap<usize, Vec<MemoryId>> = BTreeMap::new();
    for (i, row) in rows.iter().enumerate() {
        let r = root(&mut parent, i);
        groups.entry(r).or_default().push(row.id);
    }
    let mut out: Vec<Vec<MemoryId>> = groups
        .into_values()
        .filter(|members| members.len() >= 2)
        .map(|mut members| {
            members.sort_unstable();
            members
        })
        .collect();
    out.sort();
    out
}

/// Ответ тула модели.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub is_error: bool,
    pub output: Value,
}

/// Итог фазы для строки `sleep_phase_runs` и отчёта ночи.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub ops_json: String,
    pub applied: u32,
    pub discarded: u32,
    pub forgets: u32,
}

impl Default for Summary {
    fn default() -> Self {
        Self {
            ops_json: String::from("[]"),
            applied: 0,
            discarded: 0,
            forgets: 0,
        }
    }
}

struct Applied {
    output: Value,
    archived: Vec<MemoryId>,
    updated: Vec<(MemoryId, Shown)>,
    touched: Vec<MemoryId>,
}

struct Stale;

/// Фаза в работе: снапшот, счётчики, журнал операций; `call` — вход тула.
pub struct Phase<C: Components> {
    shared: Shared<C>,
    run: RunId,
    now: Timestamp,
    cfg: SleepConfig,
    snapshot: Snapshot,
    counters: Counters,
    discarded: u32,
    ops: Vec<PhaseOp>,
    /// Под `consolidated_at` по концу фазы: показанные и порождённые.
    touched: BTreeSet<MemoryId>,
    prompt: String,
}

impl<C: Components> Phase<C> {
    /// `None` — корпус меньше `sleep.min_memories`, фаза пропускается.
    pub async fn build(shared: &Shared<C>, run: RunId, now: Timestamp) -> DbResult<Option<Self>> {
        Self::build_with_dry_run(shared, run, now, shared.cfg().sleep.dry_run).await
    }

    pub async fn build_with_dry_run(
        shared: &Shared<C>,
        run: RunId,
        now: Timestamp,
        dry_run: bool,
    ) -> DbResult<Option<Self>> {
        let mut cfg = shared.cfg().sleep.clone();
        cfg.dry_run = dry_run;
        let reader = shared.db().reader().await?;
        let rows = reader.run(|c| c.memory().current()).await?;
        if rows.len() < usize::try_from(cfg.min_memories).unwrap_or(usize::MAX) {
            return Ok(None);
        }
        let vectors: HashMap<MemoryId, Vec<f32>> = match shared.cfg().memory.embed.model.clone() {
            Some(model) => reader
                .run(move |c| c.memory().vectors(&Scope::default(), &model))
                .await?
                .into_iter()
                .collect(),
            None => HashMap::new(),
        };
        let model = shared.cfg().llm.default_model.clone();
        let bytes_per_token = reader
            .run(move |c| c.turns().ratio(&model))
            .await?
            .unwrap_or(DEFAULT_BYTES_PER_TOKEN);
        let snapshot = Snapshot::build(
            &rows,
            &vectors,
            now,
            &cfg,
            &shared.cfg().memory,
            bytes_per_token,
        );
        let prompt = render(&rows, &snapshot, now);
        Ok(Some(Self {
            shared: shared.clone(),
            run,
            now,
            cfg,
            snapshot,
            counters: Counters::default(),
            discarded: 0,
            ops: Vec::new(),
            touched: BTreeSet::new(),
            prompt,
        }))
    }

    /// User-сообщение фазы.
    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub fn counters(&self) -> Counters {
        self.counters
    }

    /// Вызов тула модели: разбор → vet → применение своей транзакцией;
    /// отказ — `is_error` с причиной, тур продолжается.
    pub async fn call(&mut self, tool: &str, input: Value) -> ToolResult {
        let op = match Op::parse(tool, &input) {
            Ok(op) => op,
            Err(reason) => return self.reject(tool, input, reason),
        };
        let live = match self.live_versions(op.ids()).await {
            Ok(live) => live,
            Err(e) => {
                log::error!(tool, error:% = e; "sleep memory op failed");
                return self.reject(tool, input, format!("database error: {e}"));
            }
        };
        if let Err(reason) = vet(&op, &self.snapshot, &live, &self.counters, &self.cfg) {
            return self.reject(tool, input, reason.to_owned());
        }
        if self.cfg.dry_run {
            self.count(&op);
            self.ops.push(PhaseOp {
                tool: tool.to_owned(),
                input,
                verdict: Verdict::WouldApply,
            });
            return ToolResult {
                is_error: false,
                output: json!({ "would_apply": true }),
            };
        }
        let versions: BTreeMap<MemoryId, u32> = op
            .ids()
            .iter()
            .filter_map(|id| self.snapshot.rows.get(id).map(|s| (*id, s.version)))
            .collect();
        let (run, now, bpt, op_tx) = (
            self.run.clone(),
            self.now,
            self.snapshot.bytes_per_token,
            op.clone(),
        );
        let zone = self.shared.day_zone();
        let applied = self
            .shared
            .db()
            .write(move |tx| {
                crate::subsystems::memory::changes::track(tx, now, &zone, |tx| {
                    apply(tx, &op_tx, &versions, &run, now, bpt)
                })
            })
            .await;
        match applied {
            Ok(Ok(applied)) => {
                self.snapshot.absorb(&applied);
                self.touched.extend(applied.touched.iter().copied());
                self.count(&op);
                self.ops.push(PhaseOp {
                    tool: tool.to_owned(),
                    input,
                    verdict: Verdict::Applied,
                });
                ToolResult {
                    is_error: false,
                    output: applied.output,
                }
            }
            Ok(Err(Stale)) => self.reject(tool, input, STALE.to_owned()),
            Err(e) => {
                log::error!(tool, error:% = e; "sleep memory op failed");
                self.reject(tool, input, format!("database error: {e}"))
            }
        }
    }

    /// Конец фазы: `consolidated_at` показанным (не в `dry_run`, только если
    /// тур дошёл до `sleep_done`), метрики, `ops_json`.
    pub async fn finish(self, done: bool) -> DbResult<Summary> {
        let shared = &self.shared;
        if done && !self.cfg.dry_run {
            let ids: Vec<MemoryId> = self
                .snapshot
                .shown
                .iter()
                .chain(self.touched.iter())
                .copied()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let now = self.now;
            shared
                .db()
                .write(move |tx| tx.sleep().consolidate(&ids, now))
                .await?;
        }
        let (zone, now) = (shared.day_zone(), self.now);
        let (applied, discarded) = (self.counters.applied, self.discarded);
        shared
            .db()
            .write(move |tx| {
                let stats = tx.stats();
                stats.bump_by(
                    Metric::SleepOpsApplied,
                    "memory",
                    u64::from(applied),
                    now,
                    &zone,
                )?;
                stats.bump_by(
                    Metric::SleepOpsDiscarded,
                    "memory",
                    u64::from(discarded),
                    now,
                    &zone,
                )
            })
            .await?;
        Ok(Summary {
            ops_json: serde_json::to_string(&self.ops).unwrap_or_else(|_| String::from("[]")),
            applied,
            discarded,
            forgets: self.counters.forgets,
        })
    }

    /// Версии текущих записей сейчас: запись, закрытая хозяином, отсутствует.
    async fn live_versions(&self, ids: Vec<MemoryId>) -> DbResult<BTreeMap<MemoryId, u32>> {
        let rows = self
            .shared
            .db()
            .reader()
            .await?
            .run(move |c| c.memory().rows(&ids))
            .await?;
        Ok(rows
            .into_iter()
            .filter(|r| r.valid_to.is_none())
            .map(|r| (r.id, r.version))
            .collect())
    }

    fn count(&mut self, op: &Op) {
        self.counters.applied += 1;
        if matches!(op, Op::Forget { .. }) {
            self.counters.forgets += 1;
        }
    }

    fn reject(&mut self, tool: &str, input: Value, reason: String) -> ToolResult {
        self.discarded += 1;
        self.ops.push(PhaseOp {
            tool: tool.to_owned(),
            input,
            verdict: Verdict::Discarded {
                reason: reason.clone(),
            },
        });
        ToolResult {
            is_error: true,
            output: json!({ "error": reason }),
        }
    }
}

fn tokens_of(text: &str, bytes_per_token: f64) -> u32 {
    (text.len() as f64 / bytes_per_token).ceil() as u32
}

fn shown_of(row: &MemoryRow, version: u32, kind: Kind, bpt: f64) -> Shown {
    Shown {
        version,
        kind,
        visibility: row.visibility,
        created_at: row.created_at,
        trusted: row.source_trusted_chat == Some(true),
        tokens: tokens_of(&row.text, bpt),
    }
}

/// Применение под писателем: версии сверяются внутри транзакции, поэтому
/// параллельная запись хозяина не теряется, а устаревшая операция — `Stale`.
fn apply(
    tx: &Tx<'_>,
    op: &Op,
    versions: &BTreeMap<MemoryId, u32>,
    run: &RunId,
    now: Timestamp,
    bpt: f64,
) -> DbResult<Result<Applied, Stale>> {
    let ids = op.ids();
    let live: BTreeMap<MemoryId, MemoryRow> = tx
        .memory()
        .rows(&ids)?
        .into_iter()
        .map(|r| (r.id, r))
        .collect();
    for id in &ids {
        match live.get(id) {
            Some(row) if row.valid_to.is_none() && versions.get(id) == Some(&row.version) => {}
            _ => return Ok(Err(Stale)),
        }
    }
    let row = |id: &MemoryId| {
        live.get(id).ok_or_else(|| {
            DbError::corrupt(format_args!("memory {id} vanished inside the transaction"))
        })
    };
    let applied = match op {
        Op::Forget { id, why } => {
            tx.memory()
                .archive(*id, ArchiveReason::Forget, why, Some(run), None, now)?;
            Applied {
                output: json!({ "archived": id }),
                archived: vec![*id],
                updated: Vec::new(),
                touched: Vec::new(),
            }
        }
        Op::Merge {
            ids,
            text,
            kind,
            importance,
            why,
        } => {
            let sources: Vec<&MemoryRow> = ids.iter().map(row).collect::<DbResult<_>>()?;
            let Some(winner) = sources.iter().copied().max_by(|a, b| {
                a.access_count
                    .total_cmp(&b.access_count)
                    .then_with(|| b.created_at.cmp(&a.created_at))
                    .then_with(|| b.id.cmp(&a.id))
            }) else {
                return Ok(Err(Stale));
            };
            let merged = Merged {
                text: text.clone(),
                kind: kind.unwrap_or(winner.kind),
                visibility: sources
                    .iter()
                    .map(|r| r.visibility)
                    .max()
                    .unwrap_or(winner.visibility),
                importance: importance
                    .map(Percent::to_unit)
                    .unwrap_or_else(|| sources.iter().map(|r| r.importance).fold(0.0, f64::max)),
                created_at: sources
                    .iter()
                    .map(|r| r.created_at)
                    .min()
                    .unwrap_or(winner.created_at),
                last_reinforced_at: sources
                    .iter()
                    .map(|r| r.last_reinforced_at)
                    .max()
                    .unwrap_or(winner.last_reinforced_at),
                access_count: sources.iter().map(|r| r.access_count).sum(),
                source_trusted_chat: Some(
                    sources.iter().all(|r| r.source_trusted_chat == Some(true)),
                ),
                now,
            };
            if !tx.sleep().merge_into(winner.id, winner.version, &merged)? {
                return Ok(Err(Stale));
            }
            let mut archived = Vec::new();
            for loser in sources.iter().filter(|r| r.id != winner.id) {
                tx.memory()
                    .archive(loser.id, ArchiveReason::Merge, why, Some(run), None, now)?;
                archived.push(loser.id);
            }
            let shown = Shown {
                version: winner.version + 1,
                kind: merged.kind,
                visibility: merged.visibility,
                created_at: merged.created_at,
                trusted: merged.source_trusted_chat == Some(true),
                tokens: tokens_of(&merged.text, bpt),
            };
            Applied {
                output: json!({ "id": winner.id }),
                archived,
                updated: vec![(winner.id, shown)],
                touched: vec![winner.id],
            }
        }
        Op::Split { id, into, why } => {
            let src = row(id)?;
            let mut shards = Vec::new();
            let mut updated = Vec::new();
            for part in into {
                let kind = part.kind.unwrap_or(src.kind);
                let new = tx.memory().insert(&NewMemory {
                    kind,
                    text: part.text.clone(),
                    name: None,
                    description: None,
                    tags: src.tags.clone(),
                    visibility: src.visibility,
                    importance: part
                        .importance
                        .map(Percent::to_unit)
                        .unwrap_or(src.importance),
                    confidence: src.confidence,
                    source_actor: src.source_actor.clone(),
                    source_chat: src.source_chat,
                    source_surface: src.source_surface.clone(),
                    source_trusted_chat: src.source_trusted_chat,
                    source_run: Some(run.clone()),
                    occurred_at: src.occurred_at,
                    valid_from: now,
                    supersedes: None,
                    version: 1,
                    last_reinforced_at: src.last_reinforced_at,
                    access_count: 0.0,
                    now,
                })?;
                tx.sleep().inherit_created(new, src.created_at)?;
                tx.sleep().link_split(new, src.id, now)?;
                updated.push((
                    new,
                    Shown {
                        version: 1,
                        kind,
                        visibility: src.visibility,
                        created_at: src.created_at,
                        trusted: src.source_trusted_chat == Some(true),
                        tokens: tokens_of(&part.text, bpt),
                    },
                ));
                shards.push(new);
            }
            tx.memory()
                .archive(src.id, ArchiveReason::Split, why, Some(run), None, now)?;
            Applied {
                output: json!({ "ids": shards }),
                archived: vec![src.id],
                updated,
                touched: shards,
            }
        }
        Op::ChangeType { id, kind, .. } => match retype(tx, row(id)?, *kind, now, bpt)? {
            Ok(applied) => applied,
            Err(Stale) => return Ok(Err(Stale)),
        },
        Op::Demote { id, .. } => match retype(tx, row(id)?, Kind::Fact, now, bpt)? {
            Ok(applied) => applied,
            Err(Stale) => return Ok(Err(Stale)),
        },
        Op::Supersede { id, text, .. } => {
            let src = row(id)?;
            // Сначала закрыть: уникальный индекс имени procedure живёт только на текущих.
            tx.memory().close(src.id, now)?;
            let new = tx.memory().insert(&NewMemory {
                kind: src.kind,
                text: text.clone(),
                name: src.name.clone(),
                description: src.description.clone(),
                tags: src.tags.clone(),
                visibility: src.visibility,
                importance: src.importance,
                confidence: src.confidence,
                source_actor: src.source_actor.clone(),
                source_chat: src.source_chat,
                source_surface: src.source_surface.clone(),
                source_trusted_chat: src.source_trusted_chat,
                source_run: Some(run.clone()),
                occurred_at: src.occurred_at,
                valid_from: now,
                supersedes: Some(src.id),
                version: src.version + 1,
                last_reinforced_at: src.last_reinforced_at,
                access_count: src.access_count,
                now,
            })?;
            let mut shown = shown_of(src, src.version + 1, src.kind, bpt);
            shown.tokens = tokens_of(text, bpt);
            Applied {
                output: json!({ "id": new }),
                archived: vec![src.id],
                updated: vec![(new, shown)],
                touched: vec![new],
            }
        }
    };
    Ok(Ok(applied))
}

fn retype(
    tx: &Tx<'_>,
    row: &MemoryRow,
    kind: Kind,
    now: Timestamp,
    bpt: f64,
) -> DbResult<Result<Applied, Stale>> {
    if !tx.sleep().set_kind(row.id, row.version, kind, now)? {
        return Ok(Err(Stale));
    }
    Ok(Ok(Applied {
        output: json!({ "id": row.id, "version": row.version + 1 }),
        archived: Vec::new(),
        updated: vec![(row.id, shown_of(row, row.version + 1, kind, bpt))],
        touched: vec![row.id],
    }))
}
