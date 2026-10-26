//! `tools_find` (`02 §5.3`): BM25 по документам тулов Visible ∪ Reserved
//! текущего ctx, без модели. Документ — имя, имя словами, описание,
//! `about` группы, имена и описания полей схемы.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::subsystems::tools::{canonical, registry::Entry};

const K1: f64 = 1.2;
const B: f64 = 0.75;

pub fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Текст документа тула.
pub fn document(entry: &Entry, about: &str) -> String {
    let mut doc = format!(
        "{} {} {} {}",
        entry.canonical,
        entry.canonical.replace('_', " "),
        entry.decl.description,
        about
    );
    schema_words(&entry.decl.input, &mut doc);
    doc
}

fn schema_words(schema: &Value, out: &mut String) {
    match schema {
        Value::Object(map) => {
            if let Some(Value::Object(props)) = map.get("properties") {
                for (name, sub) in props {
                    out.push(' ');
                    out.push_str(name);
                    schema_words(sub, out);
                }
            }
            if let Some(Value::String(d)) = map.get("description") {
                out.push(' ');
                out.push_str(d);
            }
            for key in ["items", "anyOf", "oneOf", "allOf"] {
                if let Some(sub) = map.get(key) {
                    schema_words(sub, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|i| schema_words(i, out)),
        _ => {}
    }
}

/// Лучшие `limit` документов по BM25; ничья — по порядку входа (`tool_id`).
pub fn rank<'a, T>(docs: &'a [(T, String)], query: &str, limit: usize) -> Vec<&'a T> {
    let query: BTreeSet<String> = tokens(query).into_iter().collect();
    if query.is_empty() || docs.is_empty() {
        return Vec::new();
    }
    let docs_tokens: Vec<Vec<String>> = docs.iter().map(|(_, d)| tokens(d)).collect();
    let n = docs_tokens.len() as f64;
    let avg = docs_tokens.iter().map(Vec::len).sum::<usize>() as f64 / n;
    let mut df: BTreeMap<&str, usize> = BTreeMap::new();
    for doc in &docs_tokens {
        for term in doc.iter().map(String::as_str).collect::<BTreeSet<_>>() {
            *df.entry(term).or_default() += 1;
        }
    }
    let mut scored: Vec<(f64, usize)> = docs_tokens
        .iter()
        .enumerate()
        .filter_map(|(i, doc)| {
            let len = doc.len() as f64;
            let score: f64 = query
                .iter()
                .map(|term| {
                    let tf = doc.iter().filter(|t| *t == term).count() as f64;
                    if tf == 0.0 {
                        return 0.0;
                    }
                    let df = df.get(term.as_str()).copied().unwrap_or(0) as f64;
                    let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                    idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * len / avg.max(1.0)))
                })
                .sum();
            (score > 0.0).then_some((score, i))
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, i)| &docs[i].0)
        .collect()
}

/// Группа тула для `about`.
pub fn group(entry: &Entry) -> &str {
    canonical::group_of(entry.path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bm25_prefers_rare_terms_and_breaks_ties_by_order() {
        let docs = vec![
            (1, "memory recall search memory records".to_owned()),
            (2, "github issues pull requests".to_owned()),
            (3, "memory history of a record".to_owned()),
        ];
        assert_eq!(rank(&docs, "github issue", 5), vec![&2]);
        let hits = rank(&docs, "memory record", 5);
        assert_eq!(hits.len(), 2);
        assert!(rank(&docs, "weather", 5).is_empty());
        assert!(rank(&docs, "", 5).is_empty());
    }
}
