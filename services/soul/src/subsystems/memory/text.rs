//! Текстовые примитивы памяти (`03 §1`, `§4`, `§6`): запрос FTS5, триграммы и
//! Jaccard, cos, RRF, возраст строкой, slug.

use std::collections::{BTreeMap, BTreeSet};

use eva::time::Timestamp;

use crate::db::stats::DayZone;

/// Запрос `MATCH` (`03 §4` п.2): каждый терм в кавычках, через `OR`;
/// термы короче `min_chars` отброшены — trigram их не находит. Пусто —
/// искать нечего.
pub fn fts_query(query: &str, min_chars: usize) -> String {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| t.trim_matches('"'))
        .filter(|t| t.chars().count() >= min_chars.max(1))
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    terms.join(" OR ")
}

/// Триграммы символов по нормализованному тексту: регистр снят, пробелы
/// схлопнуты; текст короче трёх символов — одна «триграмма» целиком.
pub fn trigrams(text: &str) -> BTreeSet<String> {
    let flat: Vec<char> = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .chars()
        .collect();
    if flat.is_empty() {
        return BTreeSet::new();
    }
    if flat.len() < 3 {
        return BTreeSet::from([flat.into_iter().collect()]);
    }
    flat.windows(3).map(|w| w.iter().collect()).collect()
}

pub fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f64 / union as f64
}

/// Косинус; разная размерность или нулевая норма — 0.
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0_f64, 0.0_f64, 0.0_f64);
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Reciprocal rank fusion (`03 §4` п.4): `Σ 1/(k + rank)`, ранг с 1.
pub fn rrf<I: Ord + Copy>(lists: &[&[I]], k: f64) -> BTreeMap<I, f64> {
    let mut out = BTreeMap::new();
    for list in lists {
        for (rank, id) in list.iter().enumerate() {
            *out.entry(*id).or_insert(0.0) += 1.0 / (k + rank as f64 + 1.0);
        }
    }
    out
}

/// Возраст `m/h/d/w/mo/y`, минимум `1m`, дни до `13d` (`03 §5`): `1w` скрыл бы
/// разброс 7–13 дней.
pub fn age(now: Timestamp, then: Timestamp) -> String {
    let secs = now.as_secs().saturating_sub(then.as_secs());
    let mins = secs / 60;
    let hours = mins / 60;
    let days = hours / 24;
    if hours == 0 {
        format!("{}m", mins.max(1))
    } else if days == 0 {
        format!("{hours}h")
    } else if days < 14 {
        format!("{days}d")
    } else if days < 30 {
        format!("{}w", days / 7)
    } else if days < 365 {
        format!("{}mo", days / 30)
    } else {
        format!("{}y", days / 365)
    }
}

/// Дата события в сутках `system.tz` с расстоянием до `now`:
/// `2026-09-20 (5d ago)`, `(in 8d)`, `(today)`.
pub fn event_when(now: Timestamp, occurred: Timestamp, zone: &DayZone) -> String {
    let date = zone.day(occurred);
    if date == zone.day(now) {
        format!("{date} (today)")
    } else if occurred < now {
        format!("{date} ({} ago)", age(now, occurred))
    } else {
        format!("{date} (in {})", age(occurred, now))
    }
}

/// `[a-z0-9_-]{1,max}`.
pub fn is_slug(s: &str, max: usize) -> bool {
    let n = s.chars().count();
    n >= 1
        && n <= max
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_query_quotes_terms_and_drops_short_ones() {
        assert_eq!(fts_query("coffee no sugar", 3), "\"coffee\" OR \"sugar\"");
        assert_eq!(
            fts_query("coffee no sugar", 1),
            "\"coffee\" OR \"no\" OR \"sugar\""
        );
        assert_eq!(fts_query("say \"hi\" NEAR", 3), "\"say\" OR \"NEAR\"");
        assert_eq!(fts_query("a\"b", 3), "\"a\"\"b\"");
        assert_eq!(fts_query("", 3), "");
        assert_eq!(fts_query("a b", 3), "");
    }

    #[test]
    fn jaccard_of_paraphrase_is_high_and_of_other_text_low() {
        let a = trigrams("Господин пьёт кофе без сахара");
        let b = trigrams("господин пьёт  кофе без сахара, всегда");
        let c = trigrams("дедлайн по визе");
        assert!(jaccard(&a, &b) > 0.7, "{}", jaccard(&a, &b));
        assert!(jaccard(&a, &c) < 0.1);
        assert_eq!(jaccard(&a, &a), 1.0);
        assert_eq!(jaccard(&BTreeSet::new(), &BTreeSet::new()), 0.0);
        assert_eq!(trigrams("ab"), BTreeSet::from([String::from("ab")]));
    }

    #[test]
    fn cosine_edges() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-9);
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
        assert_eq!(cosine(&[1.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn rrf_sums_lists() {
        let scores = rrf(&[&[1, 2][..], &[2, 3][..]], 60.0);
        assert!((scores[&2] - (1.0 / 62.0 + 1.0 / 61.0)).abs() < 1e-12);
        assert!((scores[&1] - 1.0 / 61.0).abs() < 1e-12);
        assert!(scores[&2] > scores[&1] && scores[&1] > scores[&3]);
    }

    #[test]
    fn age_units() {
        let t0 = Timestamp::TEST_ORIGIN;
        let at = |secs: u64| t0 + std::time::Duration::from_secs(secs);
        assert_eq!(age(t0, t0), "1m");
        assert_eq!(age(at(59), t0), "1m");
        assert_eq!(age(at(5 * 60), t0), "5m");
        assert_eq!(age(at(3 * 3600), t0), "3h");
        assert_eq!(age(at(3 * 86_400), t0), "3d");
        assert_eq!(age(at(13 * 86_400), t0), "13d");
        assert_eq!(age(at(14 * 86_400), t0), "2w");
        assert_eq!(age(at(15 * 86_400), t0), "2w");
        assert_eq!(age(at(70 * 86_400), t0), "2mo");
        assert_eq!(age(at(800 * 86_400), t0), "2y");
    }

    #[test]
    fn event_dates() {
        let now = Timestamp::TEST_ORIGIN; // 2025-01-01
        let day = std::time::Duration::from_secs(86_400);
        let utc = DayZone::Utc;
        assert_eq!(event_when(now + 5 * day, now, &utc), "2025-01-01 (5d ago)");
        assert_eq!(event_when(now, now + 8 * day, &utc), "2025-01-09 (in 8d)");
        let hour = std::time::Duration::from_secs(3600);
        assert_eq!(event_when(now, now + hour, &utc), "2025-01-01 (today)");
        // Сутки режет `system.tz`: в UTC+3 23:00 UTC — уже завтра.
        let late = now + 23 * hour;
        assert_eq!(
            event_when(now, late, &DayZone::Fixed(3 * 3600)),
            "2025-01-02 (in 23h)"
        );
        assert_eq!(event_when(now, late, &utc), "2025-01-01 (today)");
    }

    #[test]
    fn slugs() {
        assert!(is_slug("deploy-eva_2", 48));
        assert!(!is_slug("Deploy", 48));
        assert!(!is_slug("", 48));
        assert!(!is_slug("a".repeat(49).as_str(), 48));
        assert!(!is_slug("с кириллицей", 48));
    }
}
