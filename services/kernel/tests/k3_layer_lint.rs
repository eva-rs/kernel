//! Слои `11 §9` грепом: SQL только в `db/`, хендлеры только в `handling/`,
//! авторизация только в `Policy`, `Caller::` в `eva-rpc` — лишь на hello.

use std::path::{Path, PathBuf};

/// Признаки SQL в коде: литерал-запрос или вызов `rusqlite`.
const SQL: &[&str] = &[
    "\"SELECT ",
    "\"INSERT ",
    "\"UPDATE ",
    "\"DELETE ",
    "\"PRAGMA ",
    "\"CREATE ",
    "\"WITH ",
    ".execute(",
    ".execute_batch(",
    ".query_row(",
    ".query_map(",
    ".prepare(",
    "rusqlite::",
];

/// SQL вне `db/`: `sqlite.*` исполняет SQL модуля в его собственном файле.
const SQL_OUTSIDE_DB: &[(&str, &str)] = &[(
    "handling/sqlite.rs",
    "sqlite.* runs the module's own statements",
)];

/// Авторизация по caller'у вне `state/policy/`: handshake выводит его из токена.
const CALLER_OUTSIDE_POLICY: &[&str] = &["subsystems/modules/auth.rs"];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

/// `(путь от base, номер строки, строка)` каждого `.rs` под `base` — до
/// `#[cfg(test)]`: юнит-тесты строят caller'ов и SQL, слоям это не мешает.
fn lines(base: &Path) -> Vec<(String, usize, String)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, usize, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let code = text
                .lines()
                .take_while(|line| line.trim() != "#[cfg(test)]");
            for (n, line) in code.enumerate() {
                out.push((rel.clone(), n + 1, line.to_owned()));
            }
        }
    }
    let mut out = Vec::new();
    walk(base, base, &mut out);
    assert!(!out.is_empty(), "nothing to lint under {}", base.display());
    out
}

fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with("//")
}

#[test]
fn sql_lives_only_in_db() {
    let src = root().join("services/kernel/src");
    let hits: Vec<String> = lines(&src)
        .into_iter()
        .filter(|(file, _, line)| {
            !file.starts_with("db/")
                && !SQL_OUTSIDE_DB.iter().any(|(allowed, _)| file == allowed)
                && !is_comment(line)
                && SQL.iter().any(|p| line.contains(p))
        })
        .map(|(file, n, line)| format!("{file}:{n}: {}", line.trim()))
        .collect();
    assert!(hits.is_empty(), "SQL outside db/ (11 §9): {hits:#?}");
}

#[test]
fn handlers_live_only_in_handling() {
    let src = root().join("services/kernel/src");
    let hits: Vec<String> = lines(&src)
        .into_iter()
        .filter(|(file, _, line)| {
            !file.starts_with("handling/") && line.trim_start().starts_with("#[eva::endpoint]")
        })
        .map(|(file, n, _)| format!("{file}:{n}"))
        .collect();
    assert!(
        hits.is_empty(),
        "#[eva::endpoint] outside handling/: {hits:#?}"
    );
}

#[test]
fn only_policy_inspects_the_caller() {
    let src = root().join("services/kernel/src");
    let hits: Vec<String> = lines(&src)
        .into_iter()
        .filter(|(file, _, line)| {
            !file.starts_with("state/policy/")
                && !CALLER_OUTSIDE_POLICY.contains(&file.as_str())
                && !is_comment(line)
                && ["Caller::", ".is_owner()", ".is_kernel()"]
                    .iter()
                    .any(|p| line.contains(p))
        })
        .map(|(file, n, line)| format!("{file}:{n}: {}", line.trim()))
        .collect();
    assert!(hits.is_empty(), "authorization outside Policy: {hits:#?}");
}

#[test]
fn rpc_names_the_caller_only_at_hello() {
    let src = root().join("rpc/src");
    let mut at_hello = Vec::new();
    let mut elsewhere = Vec::new();
    for (file, n, line) in lines(&src) {
        if is_comment(&line) || !line.contains("Caller::") {
            continue;
        }
        let hello = ["client.rs", "server/mod.rs"].contains(&file.as_str())
            && line.matches("Caller::").count() == line.matches("Caller::from_token(").count();
        if hello {
            at_hello.push(file);
        } else {
            elsewhere.push(format!("{file}:{n}: {}", line.trim()));
        }
    }
    assert!(
        elsewhere.is_empty(),
        "Caller:: in eva-rpc outside hello: {elsewhere:#?}"
    );
    at_hello.sort();
    assert_eq!(
        at_hello,
        ["client.rs", "server/mod.rs"],
        "both ends derive the caller from the token"
    );
}
