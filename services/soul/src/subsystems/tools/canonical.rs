//! Каноничное имя тула (`02 §2`): сегменты пути через `_`; длиннее лимита
//! провайдеров — голова и хэш пути. Имя — часть кэш-префикса, поэтому
//! детерминировано и стабильно между запусками.

pub use eva_protocol::types::tool::{CANONICAL_MAX as MAX, canonical};
#[cfg(test)]
const HEAD: usize = MAX - 9;
/// Длина сегмента пути.
pub const SEGMENT_MAX: usize = 24;

/// Сегмент `^[a-z][a-z0-9_]*$` не длиннее `SEGMENT_MAX`.
pub fn valid_segment(s: &str) -> bool {
    let mut chars = s.chars();
    s.len() <= SEGMENT_MAX
        && matches!(chars.next(), Some('a'..='z'))
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'))
}

/// Путь из `min..=max` сегментов.
pub fn valid_path(path: &str, min: usize, max: usize) -> bool {
    let segments: Vec<&str> = path.split('.').collect();
    (min..=max).contains(&segments.len()) && segments.iter().all(|s| valid_segment(s))
}

/// Группа тула — путь без последнего сегмента.
pub fn group_of(path: &str) -> &str {
    path.rsplit_once('.').map_or("", |(group, _)| group)
}

/// `path` равен `prefix` или лежит под ним.
pub fn under(path: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn segment() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9_]{0,23}"
    }

    fn path(max: usize) -> impl Strategy<Value = String> {
        prop::collection::vec(segment(), 2..=max).prop_map(|s| s.join("."))
    }

    proptest! {
        #[test]
        fn canonical_names_property(p in path(6), q in path(6)) {
            let a = canonical(&p);
            prop_assert!(a.len() <= MAX, "{a}");
            prop_assert!(!a.is_empty() && a.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
            prop_assert_eq!(&a, &canonical(&p), "stable");
            if p.len() <= HEAD && q.len() <= HEAD && p != q && p.replace('.', "_") != q.replace('.', "_") {
                prop_assert_ne!(&a, &canonical(&q));
            }
            if p.len() > MAX {
                prop_assert_eq!(a.len(), MAX);
                prop_assert!(a[..HEAD] == p.replace('.', "_")[..HEAD]);
            }
        }

        #[test]
        fn long_paths_with_one_prefix_differ(head in "[a-z]{21}", a in segment(), b in segment()) {
            prop_assume!(a != b);
            let base = format!("{head}.{head}.{head}");
            let (x, y) = (format!("{base}.{a}"), format!("{base}.{b}"));
            prop_assert!(x.len() > MAX && y.len() > MAX);
            prop_assert_ne!(canonical(&x), canonical(&y));
        }
    }

    #[test]
    fn short_names_join_segments() {
        assert_eq!(canonical("memory.recall"), "memory_recall");
        assert_eq!(
            canonical("mcp.github.issues.list"),
            "mcp_github_issues_list"
        );
        // коллизии склейки решает реестр, не имя
        assert_eq!(canonical("a_b.c"), canonical("a.b_c"));
        assert!(valid_path("tools.load", 2, 4));
        assert!(!valid_path("tools", 2, 4));
        assert!(!valid_path("Tools.load", 2, 4));
        assert!(!valid_path("a.b.c.d.e", 2, 4));
        assert!(under("tools.load", "tools") && under("tools", "tools") && under("x", ""));
        assert!(!under("toolsy.load", "tools"));
        assert_eq!(group_of("mcp.github.list"), "mcp.github");
    }
}
