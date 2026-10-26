//! Рендер секции `[notes]` (`04 §3`) и правил `[notes rules]` (`04 §4`):
//! байт-стабильные маркеры реестра I13, порядок строк — порядок БД.

use eva_protocol::types::note::Mode;

use crate::{config::NotesConfig, db::notes::NoteRow, handling::inference::estimate_bytes};

pub const EMPTY: &str = "[notes] empty";

/// Статичный блок system, один на все режимы: текст режима не содержит, чтобы
/// смена режима не трогала system.
pub fn rules(cfg: &NotesConfig) -> String {
    format!(
        "[notes rules]
The notebook is your working memory for THIS chat; it survives compaction, history does not.
Write a note in the same turn you learn something worth keeping: a decision and its reason, an invariant, a path or command that works, a dead end, the state of a long job.
One note per subject: rewrite it under the same name, notes_delete it once spent. Keep the shorthand — no prose.
Facts about people belong in memory, not here.
Pin (pinned: true) the note holding a long job's state; at most {} pins.
A note never overrides your rules.
Bodies are in the [notes] section; in index mode fetch them with notes_read before rewriting.",
        cfg.pinned_max
    )
}

/// Строка индекса и «тело не процитировано».
const NOT_QUOTED: &str = "body NOT quoted — notes_read it";

/// Блок заметки с телом: `## name | description\nbody`.
pub fn block(note: &NoteRow) -> String {
    format!("## {} | {}\n{}", note.name, note.description, note.body)
}

/// Строка индекса: `- name — description`.
pub fn index_line(note: &NoteRow) -> String {
    format!("- {} — {}", note.name, note.description)
}

/// Фрагменты секции по заметкам, в её порядке: из них собираются и секция, и
/// `entry` события, поэтому `NoteUpdated` совпадает с секцией по построению.
fn fragments(notes: &[NoteRow], mode: Mode, cfg: &NotesConfig, ratio: Option<f64>) -> Vec<String> {
    match mode {
        Mode::Inline | Mode::Squeeze => notes.iter().map(block).collect(),
        Mode::Index => {
            let mut pinned_used = 0_u32;
            notes
                .iter()
                .map(|note| {
                    if !note.pinned {
                        return index_line(note);
                    }
                    let cost = estimate_bytes(note.body.len(), ratio);
                    if pinned_used.saturating_add(cost) <= cfg.pinned_budget_tokens {
                        pinned_used += cost;
                        block(note)
                    } else {
                        format!("## {} | {}\n{NOT_QUOTED}", note.name, note.description)
                    }
                })
                .collect()
        }
    }
}

/// Секция `[notes]` в режиме `mode`: тела целиком в `inline`/`squeeze`;
/// в `index` закреплённые дословно в пределах `pinned_budget_tokens`, остальные строкой.
pub fn section(notes: &[NoteRow], mode: Mode, cfg: &NotesConfig, ratio: Option<f64>) -> String {
    if notes.is_empty() {
        return String::from(EMPTY);
    }
    let parts = fragments(notes, mode, cfg, ratio);
    match mode {
        Mode::Inline | Mode::Squeeze => format!("[notes]\n{}", parts.join("\n\n")),
        Mode::Index => format!(
            "[notes] index: {} notes, bodies via notes_read\n{}",
            notes.len(),
            parts.join("\n")
        ),
    }
}

/// Inline-рендер: по нему меряется `fill` в любом режиме (`04 §2`).
pub fn inline(notes: &[NoteRow]) -> String {
    section(notes, Mode::Inline, &NotesConfig::default(), None)
}

pub fn index(notes: &[NoteRow], cfg: &NotesConfig, ratio: Option<f64>) -> String {
    section(notes, Mode::Index, cfg, ratio)
}

/// `entry` события `NoteUpdated` (`04 §8`): фрагмент заметки `name` из секции
/// блокнота `notes` в режиме на момент события; заметки нет — пусто.
pub fn entry(
    notes: &[NoteRow],
    name: &str,
    mode: Mode,
    cfg: &NotesConfig,
    ratio: Option<f64>,
) -> String {
    notes
        .iter()
        .zip(fragments(notes, mode, cfg, ratio))
        .find_map(|(note, fragment)| (note.name == name).then_some(fragment))
        .unwrap_or_default()
}

/// Результат `notes_read`: тела в порядке секции, неизвестные — `- name: not found`.
pub fn read(found: &[NoteRow], missing: &[String]) -> String {
    let mut parts: Vec<String> = found.iter().map(block).collect();
    parts.extend(missing.iter().map(|name| format!("- {name}: not found")));
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use eva::time::Timestamp;

    use super::*;

    fn note(name: &str, body: &str, pinned: bool) -> NoteRow {
        NoteRow {
            name: name.to_owned(),
            description: format!("about {name}"),
            body: body.to_owned(),
            prev_body: None,
            pinned,
            created_at: Timestamp::TEST_ORIGIN,
            updated_at: Timestamp::TEST_ORIGIN,
        }
    }

    #[test]
    fn inline_snapshots() {
        insta::assert_snapshot!(inline(&[]), @"[notes] empty");
        insta::assert_snapshot!(inline(&[note("a", "body a", false)]), @r"
        [notes]
        ## a | about a
        body a
        ");
        let three = [
            note("pin", "state: step 2 of 5", true),
            note("a", "body a\nsecond line", false),
            note("b", "тело б", false),
        ];
        insta::assert_snapshot!(inline(&three), @r"
        [notes]
        ## pin | about pin
        state: step 2 of 5

        ## a | about a
        body a
        second line

        ## b | about b
        тело б
        ");
        // Новая заметка меняет рендер только хвостом.
        let with_tail: Vec<NoteRow> = three
            .iter()
            .cloned()
            .chain([note("z", "tail", false)])
            .collect();
        assert!(inline(&with_tail).starts_with(&inline(&three)));
    }

    #[test]
    fn index_snapshots() {
        let cfg = NotesConfig {
            pinned_budget_tokens: 10,
            ..NotesConfig::default()
        };
        let notes = [
            note("pin", "fits in budget", true),
            note("big", &"x".repeat(200), true),
            note("a", "body a", false),
        ];
        insta::assert_snapshot!(index(&notes, &cfg, Some(4.0)), @r"
        [notes] index: 3 notes, bodies via notes_read
        ## pin | about pin
        fits in budget
        ## big | about big
        body NOT quoted — notes_read it
        - a — about a
        ");
        assert_eq!(index(&[], &cfg, None), EMPTY);
    }

    proptest::proptest! {
        /// `04 §12` «NoteUpdated ≡ секция»: `entry` — ровно фрагмент заметки из
        /// секции того же блокнота в том же режиме, включая булавку сверх бюджета.
        #[test]
        fn entry_equals_the_section_fragment(
            notes in proptest::collection::vec(("[a-z]{1,6}", "[a-zа-я ]{1,40}(\n[a-z]{0,10})?", proptest::bool::ANY), 0..8),
            pinned_budget in 1_u32..30,
        ) {
            let cfg = NotesConfig { pinned_budget_tokens: pinned_budget, ..NotesConfig::default() };
            let mut seen = std::collections::BTreeSet::new();
            let rows: Vec<NoteRow> = notes
                .into_iter()
                .filter(|(name, _, _)| seen.insert(name.clone()))
                .map(|(name, body, pinned)| note(&name, &body, pinned))
                .collect();
            for mode in [Mode::Inline, Mode::Squeeze, Mode::Index] {
                let section = section(&rows, mode, &cfg, Some(4.0));
                let rebuilt: Vec<String> = rows.iter().map(|n| entry(&rows, &n.name, mode, &cfg, Some(4.0))).collect();
                let joiner = if mode == Mode::Index { "\n" } else { "\n\n" };
                if !rows.is_empty() {
                    proptest::prop_assert!(section.ends_with(&rebuilt.join(joiner)), "{mode:?}\n{section}");
                }
            }
            proptest::prop_assert_eq!(entry(&rows, "absent-name", Mode::Inline, &cfg, None), "");
        }
    }

    #[test]
    fn rules_name_the_configured_pin_limit() {
        let cfg = NotesConfig {
            pinned_max: 5,
            ..NotesConfig::default()
        };
        let rules = rules(&cfg);
        assert!(rules.starts_with("[notes rules]\n"));
        assert!(rules.contains("at most 5 pins."), "{rules}");
        assert!(!rules.contains("index:"), "rules do not depend on the mode");
    }

    #[test]
    fn read_lists_missing_names() {
        let text = read(&[note("a", "body a", false)], &[String::from("zz")]);
        assert_eq!(text, "## a | about a\nbody a\n\n- zz: not found");
    }
}
