//! Каталог резерва в system (`02 §5.1`), дерево `tools_list` и строки
//! операции `tools.catalog` (`02 §7`) — одна выборка поддерева.

use std::collections::BTreeMap;

use eva::data;

use eva_protocol::types::{
    ToolId,
    tool::{CatalogEntry, Exposure},
};

use crate::{
    config::tools::Catalog as CatalogConfig,
    markers::sanitize_inline,
    subsystems::tools::{
        canonical::{self, canonical},
        registry::{Catalog, Entry},
    },
};

pub const HEADER: &str = "## Reserved tools (load with tools_load)";

/// Первая фраза описания: до `. ` или конца.
pub fn first_sentence(description: &str) -> &str {
    match description.find(". ") {
        Some(end) => &description[..=end],
        None => description,
    }
}

/// Строка каталога: тул или свёрнутая группа.
struct Line {
    group: String,
    name: String,
    about: Option<String>,
}

impl Line {
    fn render(&self, with_about: bool) -> String {
        match (&self.about, with_about) {
            (Some(about), true) => format!("- {}: {about}", self.name),
            _ => format!("- {}", self.name),
        }
    }
}

/// Каталог резерва: `budget` и `tokens` — токены по калиброванному ratio.
/// Ярусы: всё; имена и описания по кругу; строки по кругу между группами с
/// хвостом `… and N more`. Возвращает текст и число выброшенных строк.
pub fn reserve(
    catalog: &Catalog,
    reserved: &[ToolId],
    cfg: &CatalogConfig,
    budget: u32,
    tokens: &dyn Fn(&str) -> u32,
) -> Option<(String, u32)> {
    let mut groups: BTreeMap<String, Vec<&Entry>> = BTreeMap::new();
    for entry in reserved.iter().filter_map(|id| catalog.get(*id)) {
        groups
            .entry(canonical::group_of(entry.path()).to_owned())
            .or_default()
            .push(entry);
    }
    if groups.is_empty() {
        return None;
    }
    let clip = |text: &str| clip(text, cfg.line_max_tokens, tokens);
    let mut lines: Vec<Line> = Vec::new();
    for (group, entries) in &groups {
        if u32::try_from(entries.len()).unwrap_or(u32::MAX) >= cfg.collapse_from.max(1) {
            let about = match catalog.about(group) {
                "" => entries
                    .iter()
                    .take(4)
                    .map(|e| e.canonical.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                about => about.to_owned(),
            };
            lines.push(Line {
                group: group.clone(),
                name: format!("{} ({} tools)", canonical(group), entries.len()),
                about: Some(clip(&sanitize_inline(&about))),
            });
        } else {
            for entry in entries {
                lines.push(Line {
                    group: group.clone(),
                    name: entry.canonical.clone(),
                    about: Some(clip(first_sentence(&entry.decl.description))),
                });
            }
        }
    }

    let total = |lines: &[&Line], about: &[bool], tail: u32| {
        let mut text = String::from(HEADER);
        for (line, with) in lines.iter().zip(about) {
            text.push('\n');
            text.push_str(&line.render(*with));
        }
        if tail > 0 {
            text.push('\n');
            text.push_str(&more(tail));
        }
        text
    };

    let all: Vec<&Line> = lines.iter().collect();
    let full = vec![true; all.len()];
    let text = total(&all, &full, 0);
    if tokens(&text) <= budget {
        return Some((text, 0));
    }

    let mut about = vec![false; all.len()];
    if tokens(&total(&all, &about, 0)) <= budget {
        for i in round_robin(&all) {
            about[i] = true;
            if tokens(&total(&all, &about, 0)) > budget {
                about[i] = false;
            }
        }
        return Some((total(&all, &about, 0), 0));
    }

    let mut kept: Vec<usize> = (0..all.len()).collect();
    let order: Vec<usize> = round_robin(&all).into_iter().rev().collect();
    for drop in order {
        kept.retain(|&i| i != drop);
        let lines: Vec<&Line> = kept.iter().map(|&i| all[i]).collect();
        let omitted = u32::try_from(all.len() - kept.len()).unwrap_or(u32::MAX);
        let text = total(&lines, &vec![false; lines.len()], omitted);
        if tokens(&text) <= budget || kept.is_empty() {
            return Some((text, omitted));
        }
    }
    None
}

/// Индексы строк по кругу между группами: по первой из каждой, потом по второй…
fn round_robin(lines: &[&Line]) -> Vec<usize> {
    let mut by_group: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, line) in lines.iter().enumerate() {
        by_group.entry(line.group.as_str()).or_default().push(i);
    }
    let mut out = Vec::with_capacity(lines.len());
    let mut round = 0;
    while out.len() < lines.len() {
        for indices in by_group.values() {
            if let Some(&i) = indices.get(round) {
                out.push(i);
            }
        }
        round += 1;
    }
    out
}

fn more(n: u32) -> String {
    format!("- … and {n} more (use tools_find)")
}

fn clip(text: &str, max: u32, tokens: &dyn Fn(&str) -> u32) -> String {
    if tokens(text) <= max {
        return text.to_owned();
    }
    let mut out: String = String::new();
    for word in text.split_whitespace() {
        let next = if out.is_empty() {
            word.to_owned()
        } else {
            format!("{out} {word}")
        };
        if tokens(&format!("{next}…")) > max {
            break;
        }
        out = next;
    }
    format!("{out}…")
}

/// Тул поддерева с его Exposure.
pub struct Listed<'a> {
    pub entry: &'a Entry,
    pub exposure: Exposure,
}

/// Поддерево `path` на `depth` уровней групп; `None` — путь неизвестен.
/// Путь тула — сам тул.
pub fn subtree<'a>(
    catalog: &'a Catalog,
    path: Option<&str>,
    depth: u8,
    exposure: &dyn Fn(&Entry) -> Exposure,
) -> Option<Vec<Listed<'a>>> {
    let prefix = path.unwrap_or("");
    let base = if prefix.is_empty() {
        0
    } else {
        prefix.split('.').count()
    };
    let mut found = catalog.groups().contains_key(prefix);
    let mut out = Vec::new();
    for entry in catalog.iter() {
        if !canonical::under(entry.path(), prefix) {
            continue;
        }
        found = true;
        let level = entry.path().split('.').count() - 1;
        if entry.path() == prefix || level.saturating_sub(base) <= usize::from(depth) {
            out.push(Listed {
                entry,
                exposure: exposure(entry),
            });
        }
    }
    (found || path.is_none()).then_some(out)
}

/// Строка операции `tools.catalog`.
pub fn entry(catalog: &Catalog, listed: &Listed<'_>, with_exposure: bool) -> CatalogEntry {
    let e = listed.entry;
    CatalogEntry {
        tool_id: e.id,
        path: e.path().to_owned(),
        canonical: e.canonical.clone(),
        description: e.decl.description.clone(),
        about: catalog.about(canonical::group_of(e.path())).to_owned(),
        needs: e.decl.needs.clone(),
        master_only: e.decl.master_only,
        trusted_only: e.decl.trusted_only,
        exposure: with_exposure.then_some(listed.exposure),
    }
}

/// Узел дерева `tools_list`.
#[data]
pub struct Group {
    pub path: String,
    pub about: String,
    pub tools: Vec<Tool>,
}

#[data]
pub struct Tool {
    pub name: String,
    pub exposure: ListedExposure,
    pub about: String,
}

#[data(copy)]
pub enum ListedExposure {
    Visible,
    Reserved,
}

/// Дерево тулу модели: Forbidden не показываются (I3).
pub fn tree(catalog: &Catalog, listed: &[Listed<'_>]) -> Vec<Group> {
    let mut groups: BTreeMap<String, Vec<Tool>> = BTreeMap::new();
    for item in listed {
        let exposure = match item.exposure {
            Exposure::Visible => ListedExposure::Visible,
            Exposure::Reserved => ListedExposure::Reserved,
            Exposure::Forbidden(_) => continue,
        };
        groups
            .entry(canonical::group_of(item.entry.path()).to_owned())
            .or_default()
            .push(Tool {
                name: item.entry.canonical.clone(),
                exposure,
                about: first_sentence(&item.entry.decl.description).to_owned(),
            });
    }
    groups
        .into_iter()
        .map(|(path, tools)| Group {
            about: catalog.about(&path).to_owned(),
            path,
            tools,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use eva_protocol::types::{ModuleId, tool::ToolDecl};
    use serde_json::json;

    use super::*;
    use crate::db::tools::ToolRow;

    fn row(id: i64, path: &str, description: &str) -> ToolRow {
        ToolRow {
            id: ToolId(id),
            module: ModuleId::from("m"),
            path: path.into(),
            canonical: canonical(path),
            decl: ToolDecl {
                path: path.into(),
                description: description.into(),
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
            online: true,
        }
    }

    fn fixture() -> (Catalog, Vec<ToolId>) {
        let mut rows = vec![row(
            1,
            "memory.history",
            "Show change history of a memory record. Slow.",
        )];
        for i in 0..14 {
            rows.push(row(10 + i, &format!("mcp.github.t{i}"), "GitHub thing."));
        }
        let groups = BTreeMap::from([(
            "mcp.github".to_owned(),
            "GitHub issues, pull requests and CI.".to_owned(),
        )]);
        let catalog = Catalog::new(rows, groups);
        let reserved = catalog.iter().map(|e| e.id).collect();
        (catalog, reserved)
    }

    fn tokens(s: &str) -> u32 {
        u32::try_from(s.len().div_ceil(4)).unwrap_or(u32::MAX)
    }

    #[test]
    fn three_budget_tiers() {
        let (catalog, reserved) = fixture();
        let cfg = CatalogConfig::default();
        let (full, omitted) = reserve(&catalog, &reserved, &cfg, 4000, &tokens).expect("catalog");
        assert_eq!(omitted, 0);
        insta::assert_snapshot!("catalog_full", full);
        assert!(full.contains("- mcp_github (14 tools): GitHub issues, pull requests and CI."));
        assert!(full.contains("- memory_history: Show change history of a memory record."));

        let names_only = tokens(
            "## Reserved tools (load with tools_load)\n- memory_history\n- mcp_github (14 tools)",
        );
        let (tier2, omitted) =
            reserve(&catalog, &reserved, &cfg, names_only, &tokens).expect("catalog");
        assert_eq!(omitted, 0);
        assert!(tier2.contains("- memory_history") && !tier2.contains("Show change"));

        let (tier3, omitted) = reserve(&catalog, &reserved, &cfg, 20, &tokens).expect("catalog");
        assert_eq!(omitted, 2);
        assert!(
            tier3.ends_with("- … and 2 more (use tools_find)"),
            "{tier3}"
        );

        let reserve_again = reserve(&catalog, &reserved, &cfg, 4000, &tokens).expect("catalog");
        assert_eq!(reserve_again.0, full, "byte-stable between runs");
    }

    #[test]
    fn group_collapses_exactly_from_collapse_from() {
        let (catalog, _) = fixture();
        let cfg = CatalogConfig {
            collapse_from: 15,
            ..CatalogConfig::default()
        };
        let reserved: Vec<ToolId> = catalog.iter().map(|e| e.id).collect();
        let (text, _) = reserve(&catalog, &reserved, &cfg, 4000, &tokens).expect("catalog");
        assert!(text.contains("- mcp_github_t0: GitHub thing."), "{text}");
        let cfg = CatalogConfig {
            collapse_from: 14,
            ..CatalogConfig::default()
        };
        let (text, _) = reserve(&catalog, &reserved, &cfg, 4000, &tokens).expect("catalog");
        assert!(text.contains("(14 tools)"), "{text}");
    }

    #[test]
    fn subtree_by_depth() {
        let (catalog, _) = fixture();
        let all = |_: &Entry| Exposure::Visible;
        assert_eq!(subtree(&catalog, None, 1, &all).expect("root").len(), 1);
        assert_eq!(subtree(&catalog, None, 2, &all).expect("root").len(), 15);
        assert_eq!(
            subtree(&catalog, Some("mcp"), 1, &all).expect("mcp").len(),
            14
        );
        assert_eq!(
            subtree(&catalog, Some("memory.history"), 1, &all)
                .expect("tool")
                .len(),
            1
        );
        assert!(subtree(&catalog, Some("nope"), 1, &all).is_none());
    }
}
