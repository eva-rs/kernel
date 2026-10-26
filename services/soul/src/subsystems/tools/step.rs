//! Снимок шага (`02 §4.1`): Visible-спеки по порядку, Reserved для каталога
//! и имена Visible ∪ Reserved. Один снимок служит спекам и отказу на
//! исполнении (I3); реестр может меняться — снимок нет.

use std::collections::BTreeMap;

use eva_protocol::types::{
    ModuleId, ToolId,
    generate::ToolSpec,
    tool::{Exposure, ToolDecl},
};

use crate::subsystems::tools::{
    canonical::{self, canonical},
    expose::{DayState, Gates, Subject, expose, expose_extra},
    registry::Catalog,
};

/// Visible тул шага.
#[derive(Debug, Clone, PartialEq)]
pub struct StepSpec {
    /// `None` — `extra_tools` тура.
    pub tool_id: Option<ToolId>,
    pub canonical: String,
    /// Локальная маршрутизация не зависит от имени в каталоге и хуках.
    pub route: String,
    /// Исполнитель тула реестра; у `extra_tools` — сессия `turns.start`.
    pub module: Option<ModuleId>,
    pub decl: ToolDecl,
}

impl StepSpec {
    /// Запись `tools[]`.
    pub fn spec(&self, deferred: bool) -> ToolSpec {
        to_spec(&self.canonical, &self.decl, deferred)
    }
}

pub fn to_spec(canonical: &str, decl: &ToolDecl, deferred: bool) -> ToolSpec {
    ToolSpec {
        name: canonical.to_owned(),
        description: decl.description.clone(),
        input_schema: decl.input.clone(),
        deferred,
    }
}

/// Имя в снимке.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Named {
    /// Индекс в `specs`.
    Visible(usize),
    Reserved(ToolId),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StepTools {
    /// Тулы реестра по `tool_id`, затем `extra_tools` в порядке объявления (`02 §4.3`).
    pub specs: Vec<StepSpec>,
    pub reserved: Vec<ToolId>,
    pub names: BTreeMap<String, Named>,
}

impl StepTools {
    pub fn visible(&self, name: &str) -> Option<&StepSpec> {
        match self.names.get(name)? {
            Named::Visible(i) => self.specs.get(*i),
            Named::Reserved(_) => None,
        }
    }

    /// До трёх ближайших имён (`02 §4.2`): Левенштейн ≤ 3 или общий префикс до `_`.
    pub fn nearest(&self, name: &str) -> Vec<String> {
        nearest(self.names.keys().map(String::as_str), name)
    }
}

/// `core` — пути `tools.core`; extra, совпавший с именем реестра, проигрывает коллизию.
pub fn snapshot(
    catalog: &Catalog,
    gates: &Gates,
    day: &DayState,
    core: &[String],
    extra: &[ToolDecl],
) -> StepTools {
    let mut out = StepTools::default();
    for entry in catalog.iter() {
        let subject = Subject {
            id: entry.id,
            decl: &entry.decl,
            online: entry.online,
            lost_collision: entry.lost_collision,
            core: core.iter().any(|p| canonical::under(entry.path(), p)),
        };
        match expose(subject, gates, day) {
            Exposure::Visible => {
                out.names
                    .insert(entry.canonical.clone(), Named::Visible(out.specs.len()));
                out.specs.push(StepSpec {
                    tool_id: Some(entry.id),
                    canonical: entry.canonical.clone(),
                    route: entry.route.clone(),
                    module: Some(entry.module.clone()),
                    decl: entry.decl.clone(),
                });
            }
            Exposure::Reserved => {
                out.names
                    .insert(entry.canonical.clone(), Named::Reserved(entry.id));
                out.reserved.push(entry.id);
            }
            Exposure::Forbidden(_) => {}
        }
    }
    for decl in extra {
        let name = canonical(&decl.path);
        if catalog.by_canonical(&name).is_some() || out.names.contains_key(&name) {
            log::error!(canonical = name.as_str(); "extra tool loses a name collision to the registry");
            continue;
        }
        if expose_extra(decl, gates) == Exposure::Visible {
            out.names
                .insert(name.clone(), Named::Visible(out.specs.len()));
            out.specs.push(StepSpec {
                tool_id: None,
                route: name.clone(),
                canonical: name,
                module: None,
                decl: decl.clone(),
            });
        }
    }
    out
}

pub fn nearest<'a>(candidates: impl Iterator<Item = &'a str>, name: &str) -> Vec<String> {
    fn prefix(s: &str) -> &str {
        s.split('_').next().unwrap_or(s)
    }
    let mut scored: Vec<(usize, &str)> = candidates
        .filter_map(|c| {
            let d = levenshtein(c, name);
            (d <= 3 || prefix(c) == prefix(name)).then_some((d, c))
        })
        .collect();
    scored.sort();
    scored
        .into_iter()
        .take(3)
        .map(|(_, c)| c.to_owned())
        .collect()
}

pub(crate) fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_by_distance_or_prefix() {
        let names = ["memory_recall", "memory_remember", "tools_find", "fs_read"];
        assert_eq!(
            nearest(names.into_iter(), "memory_recal"),
            vec!["memory_recall", "memory_remember"]
        );
        assert_eq!(nearest(names.into_iter(), "tools_fnd"), vec!["tools_find"]);
        assert!(nearest(names.into_iter(), "weather").is_empty());
        assert_eq!(levenshtein("kitten", "sitting"), 3);
    }
}
