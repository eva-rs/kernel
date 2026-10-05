//! Реестр тулов (`02 §1`): строки `tools` в памяти снимком, который шаг
//! читает без блокировок; регистрация модуля и ядра, коллизии каноничных
//! имён, проверка деклараций.
//!

use std::collections::{BTreeMap, btree_map::Entry as Slot};

use eva_protocol::{
    errors::Aux,
    types::{
        ModuleId, ToolId,
        tool::{GroupDecl, ToolDecl},
    },
};

use crate::{
    components::Components,
    config::{Config, ToolsConfig},
    db::{
        Db as _, DbResult, Queries as _,
        stats::Metric,
        tools::{KERNEL, NewTool, ToolRow},
    },
    markers,
    registry::Snapshot,
    shared::Shared,
    subsystems::tools::canonical::{self, canonical},
};

/// Тул реестра.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: ToolId,
    pub module: ModuleId,
    pub canonical: String,
    /// Имя исполнителя при регистрации; overlay меняет только видимое имя.
    pub route: String,
    pub decl: ToolDecl,
    pub online: bool,
    /// Проиграл коллизию каноничного имени тулу с меньшим `tool_id`.
    pub lost_collision: bool,
}

impl Entry {
    pub fn path(&self) -> &str {
        &self.decl.path
    }

    pub fn is_builtin(&self) -> bool {
        self.module.as_str() == KERNEL
    }
}

/// Снимок реестра.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    tools: BTreeMap<ToolId, Entry>,
    /// Каноничное имя → победитель коллизии.
    names: BTreeMap<String, ToolId>,
    groups: BTreeMap<String, String>,
}

impl Catalog {
    /// Коллизия: меньший `tool_id` побеждает (`02 §2`).
    pub fn new(mut rows: Vec<ToolRow>, groups: BTreeMap<String, String>) -> Self {
        rows.sort_by_key(|row| row.id);
        let mut tools = BTreeMap::new();
        let mut names: BTreeMap<String, ToolId> = BTreeMap::new();
        for row in rows {
            let lost_collision = match names.entry(row.canonical.clone()) {
                Slot::Vacant(slot) => {
                    slot.insert(row.id);
                    false
                }
                Slot::Occupied(_) => true,
            };
            tools.insert(
                row.id,
                Entry {
                    id: row.id,
                    module: row.module,
                    route: row.canonical.clone(),
                    canonical: row.canonical,
                    decl: row.decl,
                    online: row.online,
                    lost_collision,
                },
            );
        }
        Self {
            tools,
            names,
            groups,
        }
    }

    /// Тулы по `tool_id`.
    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.tools.values()
    }

    pub fn get(&self, id: ToolId) -> Option<&Entry> {
        self.tools.get(&id)
    }

    /// Победитель коллизии с этим каноничным именем, в т. ч. offline.
    pub fn by_canonical(&self, name: &str) -> Option<&Entry> {
        self.names.get(name).and_then(|id| self.tools.get(id))
    }

    /// `about` группы; промежуточный узел без декларации — пусто.
    pub fn about(&self, group: &str) -> &str {
        self.groups.get(group).map_or("", String::as_str)
    }

    pub fn groups(&self) -> &BTreeMap<String, String> {
        &self.groups
    }

    pub fn with_placements(
        rows: Vec<ToolRow>,
        mut groups: BTreeMap<String, String>,
        placements: BTreeMap<ToolId, String>,
    ) -> Self {
        let routes: BTreeMap<_, _> = rows.iter().map(|r| (r.id, r.canonical.clone())).collect();
        let rows = rows
            .into_iter()
            .map(|mut row| {
                if let Some(group) = placements.get(&row.id) {
                    let name = row.path.rsplit('.').next().unwrap_or(&row.path);
                    row.decl.path = format!("{group}.{name}");
                    row.canonical = canonical(&row.decl.path);
                }
                let mut group = row.decl.path.as_str();
                while let Some((parent, _)) = group.rsplit_once('.') {
                    groups.entry(parent.to_owned()).or_default();
                    group = parent;
                }
                row
            })
            .collect();
        let mut catalog = Self::new(rows, groups);
        for (id, entry) in &mut catalog.tools {
            if let Some(route) = routes.get(id) {
                entry.route.clone_from(route);
            }
        }
        catalog
    }

    /// Путь тула или группы либо каноничное имя → тулы (`02 §5.2`).
    pub fn resolve(&self, path: &str) -> Vec<&Entry> {
        if let Some(entry) = self.by_canonical(path) {
            return vec![entry];
        }
        self.tools
            .values()
            .filter(|e| canonical::under(e.path(), path))
            .collect()
    }

    /// Коллизии: пары (победитель, проигравший).
    pub fn collisions(&self) -> Vec<(&Entry, &Entry)> {
        self.tools
            .values()
            .filter(|e| e.lost_collision)
            .filter_map(|loser| self.by_canonical(&loser.canonical).map(|w| (w, loser)))
            .collect()
    }
}

pub struct Registry(Snapshot<Catalog>);

impl Default for Registry {
    fn default() -> Self {
        Self(Snapshot::new(Catalog::default()))
    }
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry").finish_non_exhaustive()
    }
}

impl Registry {
    pub fn get(&self) -> std::sync::Arc<Catalog> {
        self.0.get()
    }

    fn replace(&self, catalog: Catalog) {
        self.0.replace(catalog);
    }
}

/// Отказ декларации: модулю — `warnings`, ядру — паника теста.
pub fn check_tool(decl: &ToolDecl, cfg: &Config) -> Result<(), &'static str> {
    let tools = &cfg.tools;
    if !canonical::valid_path(&decl.path, 2, usize::from(tools.max_depth)) {
        return Err(
            "tool path must have 2..=tools.max_depth dot-separated [a-z][a-z0-9_]* segments of ≤24 chars",
        );
    }
    if decl.description.trim().is_empty() || !markers::check_inline(&decl.description) {
        return Err("description must be one non-empty line without kernel markers");
    }
    if !decl.input.is_object() {
        return Err("input must be a JSON Schema object");
    }
    if decl.timeout.is_some_and(|t| t.0 > tools.call_timeout_max.0) {
        return Err("timeout exceeds tools.call_timeout_max");
    }
    if spec_tokens(decl, cfg) > tools.max_spec_tokens {
        return Err("tool spec exceeds tools.max_spec_tokens");
    }
    Ok(())
}

pub fn check_group(group: &GroupDecl, tools: &ToolsConfig) -> Result<(), &'static str> {
    let depth = usize::from(tools.max_depth).saturating_sub(1).max(1);
    if !canonical::valid_path(&group.path, 1, depth) {
        return Err("group path must be dot-separated [a-z][a-z0-9_]* segments");
    }
    if !markers::check_inline(&group.about) {
        return Err("about must be one line without kernel markers");
    }
    Ok(())
}

/// Токены спеки по стартовому ratio: проверка регистрации без калибровки чата.
fn spec_tokens(decl: &ToolDecl, cfg: &Config) -> u32 {
    let bytes = decl.path.len() + decl.description.len() + decl.input.to_string().len();
    let ratio = cfg.context.ratio_default.max(1.0);
    (bytes as f64 / ratio).ceil() as u32
}

/// `blake3` схемы и описания: смена спеки видна без сравнения JSON.
fn spec_hash(decl: &ToolDecl) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher
        .update(decl.description.as_bytes())
        .update(&[0])
        .update(crate::subsystems::turns::dedup::canonical_json(&decl.input).as_bytes());
    *hasher.finalize().as_bytes()
}

/// Декларации модуля (или ядра) в реестр: повторная — тот же `tool_id`,
/// исчезнувший путь — `retired_at`; затем снимок перечитывается.
pub async fn register<C: Components>(
    shared: &Shared<C>,
    module: &ModuleId,
    groups: &[GroupDecl],
    tools: &[ToolDecl],
) -> Result<Vec<ToolId>, Aux> {
    let now = shared.now();
    let module = module.clone();
    let rows: Vec<(ToolDecl, String, [u8; 32])> = tools
        .iter()
        .map(|d| (d.clone(), canonical(&d.path), spec_hash(d)))
        .collect();
    let groups = groups.to_vec();
    let ids = shared
        .db()
        .write(move |tx| {
            let mut ids = Vec::with_capacity(rows.len());
            for (decl, canonical, hash) in &rows {
                ids.push(tx.tools().upsert(
                    &NewTool {
                        module: &module,
                        decl,
                        canonical,
                        spec_hash: hash,
                    },
                    now,
                )?);
            }
            tx.tools().retire_missing(&module, &ids, now)?;
            for group in &groups {
                tx.tools()
                    .put_module_group(&group.path, &group.about, now)?;
            }
            DbResult::Ok(ids)
        })
        .await?;
    reload(shared).await?;
    Ok(ids)
}

/// Модуль ушёл: его тулы offline, байты `tools[]` сегментов не трогаются.
pub async fn offline<C: Components>(shared: &Shared<C>, module: &ModuleId) -> Result<(), Aux> {
    let module = module.clone();
    shared
        .db()
        .write(move |tx| tx.tools().set_offline(&module))
        .await?;
    reload(shared).await
}

/// Снимок из БД; новые коллизии — в лог и метрику.
pub async fn reload<C: Components>(shared: &Shared<C>) -> Result<(), Aux> {
    let (rows, groups, placements) = shared
        .db()
        .reader()
        .await?
        .run(|c| {
            DbResult::Ok((
                c.tools().live()?,
                c.tools().groups()?,
                c.tools().placements()?,
            ))
        })
        .await?;
    let catalog = Catalog::with_placements(rows, groups, placements);
    let before = shared.tools().get();
    for (winner, loser) in catalog.collisions() {
        if before.get(loser.id).is_some_and(|e| e.lost_collision) {
            continue;
        }
        log::error!(canonical = winner.canonical.as_str(), winner = winner.path(), loser = loser.path(), loser_module:% = loser.module; "tool name collision: the smaller tool_id wins");
        shared
            .bump(Metric::ToolsCollisions, loser.canonical.as_str())
            .await;
    }
    shared.tools().replace(catalog);
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn row(id: i64, path: &str, online: bool) -> ToolRow {
        ToolRow {
            id: ToolId(id),
            module: ModuleId::from("m"),
            path: path.into(),
            canonical: canonical(path),
            decl: ToolDecl {
                path: path.into(),
                description: "A tool.".into(),
                input: json!({"type": "object", "properties": {}}),
                master_only: false,
                trusted_only: false,
                needs: vec![],
                ends_turn: false,
                parallel_safe: false,
                waits_on_cancel: false,
                result: Default::default(),
                timeout: None,
                idempotent: false,
            },
            online,
        }
    }

    #[test]
    fn collision_loser_is_the_larger_tool_id() {
        let catalog = Catalog::new(
            vec![
                row(3, "a_b.c", true),
                row(5, "a.b_c", true),
                row(7, "x.y", false),
            ],
            BTreeMap::new(),
        );
        let winner = catalog.by_canonical("a_b_c").expect("winner");
        assert_eq!(winner.id, ToolId(3));
        assert!(catalog.get(ToolId(5)).expect("loser").lost_collision);
        assert_eq!(catalog.collisions().len(), 1);
        assert_eq!(catalog.resolve("x").len(), 1);
        assert_eq!(catalog.resolve("x_y")[0].id, ToolId(7));
        assert!(catalog.resolve("nope").is_empty());
    }
}
