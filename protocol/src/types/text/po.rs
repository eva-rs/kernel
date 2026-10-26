//! PO читается строго: повреждённый каталог не превращается в тихий fallback.

use super::{Catalog, Translation, placeholders, plural};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
struct Entry {
    context: Option<String>,
    id: Option<String>,
    plural: Option<String>,
    forms: BTreeMap<usize, String>,
    fuzzy: bool,
}
#[derive(Clone, Copy)]
enum Field {
    Context,
    Id,
    Plural,
    Form(usize),
}

impl Entry {
    fn value(&mut self, field: Field) -> &mut String {
        match field {
            Field::Context => self.context.get_or_insert_default(),
            Field::Id => self.id.get_or_insert_default(),
            Field::Plural => self.plural.get_or_insert_default(),
            Field::Form(n) => self.forms.entry(n).or_default(),
        }
    }
}

pub(super) fn parse(source: &str, language: &str) -> Result<Catalog, String> {
    let mut entries = Vec::new();
    let mut entry = Entry::default();
    let mut active = None;
    for (line_no, raw) in source.lines().enumerate() {
        let line = raw.trim();
        let fail = |why: &str| format!("PO line {}: {why}", line_no + 1);
        if line.is_empty() {
            if entry.id.is_some() {
                entries.push(std::mem::take(&mut entry));
            }
            active = None;
            continue;
        }
        if line.starts_with("#~") {
            continue;
        }
        if line.starts_with('#') {
            if entry.id.is_some() && !entry.forms.is_empty() {
                entries.push(std::mem::take(&mut entry));
                active = None;
            }
            if let Some(flags) = line.strip_prefix("#,") {
                entry.fuzzy |= flags.split(',').any(|flag| flag.trim() == "fuzzy");
            }
            continue;
        }
        let (field, quoted, continuation) = if line.starts_with('"') {
            (
                active.ok_or_else(|| fail("string without a field"))?,
                line,
                true,
            )
        } else if let Some(value) = line.strip_prefix("msgctxt ") {
            if entry.id.is_some() {
                entries.push(std::mem::take(&mut entry));
            }
            (Field::Context, value, false)
        } else if let Some(value) = line.strip_prefix("msgid_plural ") {
            (Field::Plural, value, false)
        } else if let Some(value) = line.strip_prefix("msgid ") {
            if entry.id.is_some() {
                entries.push(std::mem::take(&mut entry));
            }
            (Field::Id, value, false)
        } else if let Some(value) = line.strip_prefix("msgstr ") {
            (Field::Form(0), value, false)
        } else if let Some(value) = line.strip_prefix("msgstr[") {
            let (index, value) = value
                .split_once(']')
                .ok_or_else(|| fail("invalid msgstr index"))?;
            let index: usize = index.parse().map_err(|_| fail("invalid msgstr index"))?;
            if index >= 16 {
                return Err(fail("too many plural forms"));
            }
            (Field::Form(index), value.trim(), false)
        } else {
            return Err(fail("unknown PO directive"));
        };
        let value = unquote(quoted).map_err(|why| fail(&why))?;
        if !continuation {
            let present = match field {
                Field::Context => entry.context.is_some(),
                Field::Id => entry.id.is_some(),
                Field::Plural => entry.plural.is_some(),
                Field::Form(n) => entry.forms.contains_key(&n),
            };
            if present {
                return Err(fail("duplicate PO field"));
            }
        }
        entry.value(field).push_str(&value);
        active = Some(field);
    }
    if entry.id.is_some() {
        entries.push(entry);
    }
    let mut catalog = Catalog {
        language: language.into(),
        nplurals: 2,
        plural: "n != 1".into(),
        entries: Vec::new(),
    };
    let mut header_seen = false;
    for entry in &entries {
        if entry.id.as_deref() != Some("") || entry.context.is_some() {
            continue;
        }
        if header_seen {
            return Err("duplicate PO header".into());
        }
        header_seen = true;
        let header = entry.forms.get(&0).ok_or("PO header has no msgstr")?;
        for line in header.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            if key.eq_ignore_ascii_case("Language") && !value.trim().is_empty() {
                catalog.language = value.trim().into();
            }
            if key.eq_ignore_ascii_case("Plural-Forms") {
                for field in value.split(';') {
                    if let Some((key, value)) = field.split_once('=') {
                        match key.trim() {
                            "nplurals" => {
                                catalog.nplurals =
                                    value.trim().parse().map_err(|_| "invalid nplurals")?
                            }
                            "plural" => catalog.plural = value.trim().into(),
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    if catalog.nplurals == 0 || catalog.nplurals > 16 {
        return Err("nplurals must be 1..=16".into());
    }
    for n in (0..=200).chain([u64::MAX]) {
        let form = plural::evaluate(&catalog.plural, n)?;
        if form < 0 || form >= catalog.nplurals as i128 {
            return Err("plural expression returned an invalid form".into());
        }
    }
    let mut seen = BTreeSet::new();
    for entry in entries {
        let Some(id) = entry.id else {
            continue;
        };
        if (id.is_empty() && entry.context.is_none()) || entry.fuzzy {
            continue;
        }
        if !seen.insert((entry.context.clone(), id.clone())) {
            return Err(format!("duplicate PO entry: {id:?}"));
        }
        let expected = if entry.plural.is_some() {
            catalog.nplurals
        } else {
            1
        };
        if entry.forms.len() != expected || (0..expected).any(|n| !entry.forms.contains_key(&n)) {
            return Err(format!("expected {expected} translation forms for {id:?}"));
        }
        let mut allowed = placeholders(&id)?;
        if let Some(plural) = &entry.plural {
            allowed.extend(placeholders(plural)?);
        }
        let forms: Vec<_> = entry.forms.into_values().collect();
        for form in &forms {
            if !placeholders(form)?.is_subset(&allowed) {
                return Err(format!("unknown parameter in translation of {id:?}"));
            }
        }
        catalog.entries.push(Translation {
            id,
            context: entry.context,
            plural: entry.plural,
            forms,
        });
    }
    Ok(catalog)
}

fn unquote(value: &str) -> Result<String, String> {
    let value = value
        .trim()
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .ok_or("expected quoted PO string")?;
    let mut chars = value.chars();
    let mut out = String::new();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            if ch == '"' {
                return Err("unescaped quote in PO string".into());
            }
            out.push(ch);
            continue;
        }
        out.push(match chars.next().ok_or("incomplete PO escape")? {
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'b' => '\u{8}',
            'f' => '\u{c}',
            'v' => '\u{b}',
            'a' => '\u{7}',
            '\\' => '\\',
            '"' => '"',
            _ => return Err("unsupported PO escape".into()),
        });
    }
    Ok(out)
}
