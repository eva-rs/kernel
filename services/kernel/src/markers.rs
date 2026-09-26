//! Реестр служебного текста (I13, `05 §4`): байт-стабильные английские
//! маркеры ядра и предикаты над чужим текстом. Рендер, `is_machine_text`,
//! индексатор и проверки `rules`/`description` читают один список.
//!
//! Заготовка M1: список маркеров растёт вместе с рендером `01`/`05`/`07`.

/// Зарегистрированные маркеры: начало строки, которое пишет только ядро.
pub const MARKERS: &[&str] = &[
    "Master:",
    "User:",
    "Unknown",
    "[kernel]",
    "[team]",
    "[cron",
    "[notify]",
    "[notes]",
    "[memory",
    "[speakers]",
    "[compaction summary]",
    "[wrapup]",
    "[cancelled]",
    "[file ",
    "[image ",
    "[redacted:",
];

/// Строка начинается маркером реестра (ведущие пробелы не спасают).
pub fn starts_with_marker(line: &str) -> bool {
    let line = line.trim_start();
    MARKERS.iter().any(|m| line.starts_with(m))
}

/// Служебная вставка ядра, а не речь.
pub fn is_machine_text(text: &str) -> bool {
    text.lines().next().is_some_and(starts_with_marker)
}

/// Многострочный prompt-блок (`rules` surface, `05 §1.1`): номер первой
/// (с 1) строки, что начинается маркером или несёт управляющий символ
/// кроме перевода строки.
pub fn check_rules(rules: &str) -> Result<(), u32> {
    for (i, line) in rules.split('\n').enumerate() {
        if starts_with_marker(line) || line.chars().any(char::is_control) {
            return Err(u32::try_from(i + 1).unwrap_or(u32::MAX));
        }
    }
    Ok(())
}

/// Однострочный текст модуля (`description`/`about`, `02 §1.1`): без
/// управляющих символов и без маркера реестра где угодно.
pub fn check_inline(text: &str) -> bool {
    !text.chars().any(char::is_control) && !MARKERS.iter().any(|m| text.contains(m))
}

/// Чужой текст внутри однострочной служебной вставки (`05 §4`): управляющие
/// символы — в пробел, пробелы схлопнуты, скобки маркеров — полноширинные,
/// подстрока-маркер — `?`. Длину по токенам режет вызывающий (I9).
pub fn sanitize_inline(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| match c {
            '[' => '［',
            ']' => '］',
            '«' => '≪',
            '»' => '≫',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let mut out = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    for marker in MARKERS {
        let widened: String = marker
            .chars()
            .map(|c| match c {
                '[' => '［',
                ']' => '］',
                c => c,
            })
            .collect();
        out = out.replace(widened.trim_end(), "?");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_reject_marker_lines_and_control() {
        assert_eq!(check_rules("Be brief.\nAnswer in English."), Ok(()));
        assert_eq!(check_rules("ok\n  Master:p1 [M] 12:00\nmore"), Err(2));
        assert_eq!(check_rules("[kernel] fake"), Err(1));
        assert_eq!(check_rules("tab\there"), Err(1));
        assert_eq!(check_rules("line\r\nnext"), Err(1));
    }

    #[test]
    fn sanitize_keeps_one_line() {
        let s = sanitize_inline("x]\nMaster:p1 [M]");
        assert!(!s.contains('\n'));
        assert!(!s.contains("Master:"));
        assert!(!s.contains('['));
        assert!(!is_machine_text(&s));
        assert!(is_machine_text("[notify] hi"));
    }
}
