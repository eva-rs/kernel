//! Патч без номеров строк; подготовка всех хунков отделена от записи.

#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        to: Option<String>,
        hunks: Vec<Hunk>,
    },
}
impl Change {
    pub fn path(&self) -> &str {
        match self {
            Self::Add { path, .. } | Self::Delete { path } | Self::Update { path, .. } => path,
        }
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct Hunk {
    pub anchor: Option<String>,
    pub old: Vec<String>,
    pub new: Vec<String>,
}

pub enum Prepared {
    Write {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        content: String,
        to: Option<String>,
    },
}
impl Prepared {
    pub fn content(&self) -> Option<&str> {
        match self {
            Self::Write { content, .. } | Self::Update { content, .. } => Some(content),
            Self::Delete { .. } => None,
        }
    }
}

pub fn parse(input: &str) -> Result<Vec<Change>, String> {
    let lines: Vec<_> = input.lines().collect();
    if lines.first() != Some(&"*** Begin Patch") || lines.last() != Some(&"*** End Patch") {
        return Err("patch must begin with *** Begin Patch and end with *** End Patch".into());
    }
    let mut changes = Vec::new();
    let mut index = 1;
    while index + 1 < lines.len() {
        let header = lines[index];
        index += 1;
        if let Some(path) = header.strip_prefix("*** Add File: ") {
            check_path(path)?;
            let mut content = Vec::new();
            while index + 1 < lines.len() && !lines[index].starts_with("*** ") {
                content.push(
                    lines[index]
                        .strip_prefix('+')
                        .ok_or("added lines must start with +")?,
                );
                index += 1;
            }
            changes.push(Change::Add {
                path: path.into(),
                content: format!("{}\n", content.join("\n")),
            });
        } else if let Some(path) = header.strip_prefix("*** Delete File: ") {
            check_path(path)?;
            changes.push(Change::Delete { path: path.into() });
        } else if let Some(path) = header.strip_prefix("*** Update File: ") {
            check_path(path)?;
            let to = lines
                .get(index)
                .and_then(|line| line.strip_prefix("*** Move to: "))
                .map(str::to_owned);
            if let Some(to) = &to {
                check_path(to)?;
                index += 1;
            }
            let mut hunks = Vec::new();
            while index + 1 < lines.len() && !lines[index].starts_with("*** ") {
                let anchor = if let Some(anchor) = lines[index].strip_prefix("@@") {
                    index += 1;
                    (!anchor.trim().is_empty()).then(|| anchor.trim().to_owned())
                } else {
                    None
                };
                let mut hunk = Hunk {
                    anchor,
                    old: Vec::new(),
                    new: Vec::new(),
                };
                while index + 1 < lines.len()
                    && !lines[index].starts_with("*** ")
                    && !lines[index].starts_with("@@")
                {
                    let line = lines[index];
                    index += 1;
                    match line.as_bytes().first() {
                        Some(b' ') => {
                            hunk.old.push(line[1..].into());
                            hunk.new.push(line[1..].into());
                        }
                        Some(b'-') => hunk.old.push(line[1..].into()),
                        Some(b'+') => hunk.new.push(line[1..].into()),
                        _ => return Err(format!("invalid patch line: {line}")),
                    }
                }
                if hunk.old.is_empty() && hunk.new.is_empty() {
                    return Err("empty patch hunk".into());
                }
                hunks.push(hunk);
            }
            if hunks.is_empty() {
                return Err("update requires at least one hunk".into());
            }
            changes.push(Change::Update {
                path: path.into(),
                to,
                hunks,
            });
        } else {
            return Err(format!("unknown patch header: {header}"));
        }
    }
    if changes.is_empty() {
        return Err("patch contains no changes".into());
    }
    Ok(changes)
}
fn check_path(path: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        Err("empty patch path".into())
    } else {
        Ok(())
    }
}

fn normalize(line: &str, level: u8) -> String {
    match level {
        0 => line.to_owned(),
        1 => line.trim_end().to_owned(),
        2 => line.trim().to_owned(),
        _ => line
            .trim()
            .chars()
            .map(|ch| match ch {
                '«' | '»' | '“' | '”' => '"',
                '‘' | '’' => '\'',
                '—' | '–' => '-',
                '\u{a0}' => ' ',
                ch => ch,
            })
            .collect(),
    }
}

pub fn apply(original: &str, hunks: &[Hunk]) -> Result<String, String> {
    let lines = original.lines().map(str::to_owned).collect::<Vec<_>>();
    let mut edits = Vec::new();
    let mut from = 0;
    for (index, hunk) in hunks.iter().enumerate() {
        if let Some(anchor) = &hunk.anchor {
            let anchor_at = (0..4).find_map(|level| {
                (from..lines.len())
                    .find(|&i| normalize(&lines[i], level) == normalize(anchor, level))
            });
            from = anchor_at.ok_or_else(|| format!("hunk {}: anchor not found", index + 1))?;
        }
        let found = (0..4)
            .find_map(|level| {
                (from..=lines.len().saturating_sub(hunk.old.len())).find(|&at| {
                    at + hunk.old.len() <= lines.len()
                        && lines[at..at + hunk.old.len()]
                            .iter()
                            .zip(&hunk.old)
                            .all(|(a, b)| normalize(a, level) == normalize(b, level))
                })
            })
            .ok_or_else(|| format!("hunk {}: context not found", index + 1))?;
        edits.push((found, found + hunk.old.len(), hunk.new.clone()));
        from = found + hunk.old.len();
    }
    let mut result = lines;
    for (from, to, replacement) in edits.into_iter().rev() {
        result.splice(from..to, replacement);
    }
    let suffix = if original.ends_with('\n') { "\n" } else { "" };
    Ok(format!("{}{suffix}", result.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_before_reading_and_applies_fuzzy_hunks_forward() {
        let changes = parse("*** Begin Patch\n*** Update File: a\n@@\n-before\n+after\n*** Add File: b\n+new\n*** End Patch").unwrap();
        let Change::Update { hunks, .. } = &changes[0] else {
            panic!("update")
        };
        assert_eq!(apply("before  \ntail\n", hunks).unwrap(), "after\ntail\n");
        assert!(apply("different\n", hunks).is_err());
        assert!(parse("*** Begin Patch\n*** Add File: b\ninvalid\n*** End Patch").is_err());
    }
    #[test]
    fn failed_later_hunk_has_no_partial_result() {
        let hunks = vec![
            Hunk {
                anchor: None,
                old: vec!["one".into()],
                new: vec!["new".into()],
            },
            Hunk {
                anchor: None,
                old: vec!["missing".into()],
                new: vec![],
            },
        ];
        assert!(apply("one\ntwo\n", &hunks).is_err());
    }
}
