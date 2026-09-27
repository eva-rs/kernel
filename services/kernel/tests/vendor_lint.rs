//! I14 (`06 §12`, `11 §9`): ни одной строки вендора или поверхности вне
//! `providers/*` — ни в коде, ни в тестах, SQL, манифестах и снапшотах.

use std::path::{Path, PathBuf};

/// Список-запрет, без учёта регистра.
const BANNED: &[&str] = &[
    "anthropic",
    "openai",
    "openrouter",
    "claude",
    "gemini",
    "mistral",
    "ollama",
    "telegram",
    "whatsapp",
    "discord",
    "slack",
];

/// Крейты, в которых vendor-кода быть не может.
const CRATES: &[&str] = &[
    "protocol",
    "macros",
    "core",
    "rpc",
    "sdk",
    "services/kernel",
];

fn walk(dir: &Path, skip: &Path, hits: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == skip {
            continue;
        }
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            walk(&path, skip, hits);
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            let lower = line.to_lowercase();
            for word in BANNED.iter().filter(|w| lower.contains(*w)) {
                hits.push(format!("{}:{}: {word}", path.display(), n + 1));
            }
        }
    }
}

#[test]
fn no_vendor_strings_outside_providers() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .expect("manifest dir");
    let root = manifest
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    let this: PathBuf = manifest.join("tests").join("vendor_lint.rs");

    let mut hits = Vec::new();
    for krate in CRATES {
        walk(&root.join(krate), &this, &mut hits);
    }
    assert!(hits.is_empty(), "vendor strings (I14): {hits:#?}");
}
