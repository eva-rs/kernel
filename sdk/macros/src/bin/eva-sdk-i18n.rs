//! Извлечение msg! в POT; обновление переводов выполняет стандартный msgmerge.

use eva::{cli::App, data, supervisor::Supervisor};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use syn::visit::Visit;

#[path = "../message.rs"]
mod message;

type Key = (Option<String>, String);
#[derive(Default)]
struct Entry {
    plural: Option<String>,
    files: BTreeSet<String>,
}
struct Extractor<'a> {
    file: &'a str,
    entries: &'a mut BTreeMap<Key, Entry>,
    errors: Vec<String>,
}

impl<'ast> Visit<'ast> for Extractor<'_> {
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if mac
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "msg")
        {
            match syn::parse2::<message::Input>(mac.tokens.clone()) {
                Ok(input) => {
                    let key = (input.context.map(|s| s.value()), input.id.value());
                    let plural = input.plural.map(|s| s.value());
                    let entry = self.entries.entry(key).or_default();
                    if !entry.files.is_empty() && entry.plural != plural {
                        self.errors
                            .push(format!("{}: conflicting plural forms", self.file));
                    }
                    entry.plural = plural;
                    entry.files.insert(self.file.into());
                }
                Err(e) => self.errors.push(format!("{}: {e}", self.file)),
            }
        }
        syn::visit::visit_macro(self, mac);
    }
}

fn files(root: &Path, found: &mut Vec<PathBuf>) -> Result<(), String> {
    if root.is_file() {
        if root.extension().is_some_and(|ext| ext == "rs") {
            found.push(root.into());
        }
        return Ok(());
    }
    for entry in std::fs::read_dir(root).map_err(|e| format!("{}: {e}", root.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        if matches!(entry.file_name().to_str(), Some("target" | ".git")) {
            continue;
        }
        if entry.file_type().map_err(|e| e.to_string())?.is_symlink() {
            continue;
        }
        files(&entry.path(), found)?;
    }
    Ok(())
}

fn extract(root: &Path) -> Result<String, String> {
    let mut paths = Vec::new();
    files(root, &mut paths)?;
    paths.sort();
    let mut entries = BTreeMap::new();
    for path in paths {
        let source =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let syntax = syn::parse_file(&source).map_err(|e| format!("{}: {e}", path.display()))?;
        let shown = path.to_string_lossy();
        let mut extractor = Extractor {
            file: &shown,
            entries: &mut entries,
            errors: Vec::new(),
        };
        extractor.visit_file(&syntax);
        if !extractor.errors.is_empty() {
            return Err(extractor.errors.join("\n"));
        }
    }
    let mut output = String::from(
        "msgid \"\"\nmsgstr \"\"\n\"Content-Type: text/plain; charset=UTF-8\\n\"\n\"Plural-Forms: nplurals=2; plural=n != 1;\\n\"\n",
    );
    for ((context, id), entry) in entries {
        output.push('\n');
        for file in entry.files {
            output.push_str(&format!("#: {file}\n"));
        }
        if let Some(context) = context {
            output.push_str(&format!("msgctxt {}\n", quoted(&context)?));
        }
        output.push_str(&format!("msgid {}\n", quoted(&id)?));
        if let Some(plural) = entry.plural {
            output.push_str(&format!(
                "msgid_plural {}\nmsgstr[0] \"\"\nmsgstr[1] \"\"\n",
                quoted(&plural)?
            ));
        } else {
            output.push_str("msgstr \"\"\n");
        }
    }
    Ok(output)
}
fn quoted(s: &str) -> Result<String, String> {
    serde_json::to_string(s).map_err(|e| e.to_string())
}

#[derive(Clone, clap::Args)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, clap::Subcommand)]
enum Command {
    /// Extract msg! calls from Rust sources into a POT template.
    Extract { root: PathBuf, output: PathBuf },
    /// Update an existing PO catalog using GNU msgmerge.
    Merge { po: PathBuf, pot: PathBuf },
}

#[data]
#[derive(Default)]
struct ToolingConfig {}

fn run(args: &Args) -> Result<(), String> {
    match &args.command {
        Command::Extract { root, output } => {
            let pot = extract(root)?;
            std::fs::write(output, pot).map_err(|e| format!("{}: {e}", Path::new(output).display()))
        }
        Command::Merge { po, pot } => {
            let status = std::process::Command::new("msgmerge")
                .args(["--update", "--backup=none", "--"])
                .arg(po)
                .arg(pot)
                .status()
                .map_err(|e| format!("cannot run GNU msgmerge: {e}"))?;
            if status.success() {
                Ok(())
            } else {
                Err(format!("msgmerge exited with {status}"))
            }
        }
    }
}
fn main() -> eyre::Result<()> {
    App::default()
        .optional::<ToolingConfig>("i18n")
        .run(async |args: Args, configs| {
            let (send, receive) = std::sync::mpsc::channel();
            Supervisor::new(configs)
                .add::<ToolingConfig>(move |_, _| {
                    let args = args.clone();
                    let send = send.clone();
                    async move {
                        let _ = send.send(run(&args));
                        Ok(())
                    }
                })
                .wait_for_completion()
                .await?;
            receive.recv()?.map_err(eyre::Error::msg)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extraction_keeps_context_plural_and_rejects_missing_parameters() {
        let syntax = syn::parse_file(
            r#"fn a() {
            msg!("Hello {name}", name = name);
            msg!("Open", context = "button");
            msg!("{count} file", plural = "{count} files", count = 3);
        }"#,
        )
        .unwrap();
        let mut entries = BTreeMap::new();
        let mut extractor = Extractor {
            file: "a.rs",
            entries: &mut entries,
            errors: Vec::new(),
        };
        extractor.visit_file(&syntax);
        assert!(extractor.errors.is_empty());
        assert_eq!(entries.len(), 3);
        assert!(entries.contains_key(&(Some("button".into()), "Open".into())));
        assert!(syn::parse_str::<message::Input>(r#""Hi {name}""#).is_err());
        assert!(syn::parse_str::<message::Input>(r#""Hi", name = "a""#).is_err());
    }
}
