//! Переводимые сообщения и gettext-каталоги без I/O; язык выбирает получатель.

use eva::data;
use std::collections::{BTreeMap, BTreeSet};

mod plural;
mod po;

/// Ограничивает раскрытие параметров недоверенного сообщения.
const MAX_TEXT: usize = 64 * 1024;

#[data]
pub struct Message {
    pub id: String,
    pub context: Option<String>,
    pub plural: Option<String>,
    pub count: Option<u64>,
    #[serde(default)]
    pub args: BTreeMap<String, String>,
}

impl Message {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            context: None,
            plural: None,
            count: None,
            args: BTreeMap::new(),
        }
    }
    pub fn context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }
    pub fn arg(mut self, name: impl Into<String>, value: impl std::fmt::Display) -> Self {
        self.args.insert(name.into(), value.to_string());
        self
    }
    pub fn plural(mut self, plural: impl Into<String>, count: u64) -> Self {
        self.plural = Some(plural.into());
        self.count = Some(count);
        self.args.insert("count".into(), count.to_string());
        self
    }
    pub fn fallback(&self) -> String {
        let template = if self.count.is_some_and(|n| n != 1) {
            self.plural.as_deref().unwrap_or(&self.id)
        } else {
            &self.id
        };
        interpolate(template, &self.args)
    }
}

/// Только переводы этой строки; полный каталог не нужен подписчику ленты.
#[data]
pub struct LocalizedText {
    pub domain: String,
    pub revision: String,
    pub message: Message,
    pub translations: BTreeMap<String, String>,
}

impl LocalizedText {
    pub fn valid(&self) -> bool {
        self.domain.len() <= 128
            && self.revision.len() <= 128
            && self.translations.len() <= 128
            && self.message.args.len() <= 128
            && self.message.id.len() <= MAX_TEXT
            && self
                .message
                .plural
                .as_ref()
                .is_none_or(|s| s.len() <= MAX_TEXT)
            && self
                .message
                .context
                .as_ref()
                .is_none_or(|s| s.len() <= MAX_TEXT)
            && self
                .message
                .args
                .iter()
                .all(|(k, v)| k.len() <= 128 && v.len() <= MAX_TEXT)
            && self
                .translations
                .iter()
                .all(|(k, v)| k.len() <= 128 && v.len() <= MAX_TEXT)
    }

    pub fn render(&self, locale: &str) -> String {
        for locale in locale_chain(locale) {
            if let Some(template) = self.translations.get(&locale) {
                return interpolate(template, &self.message.args);
            }
        }
        self.message.fallback()
    }

    /// Soul проверяет и шаблоны, и раскрытый текст, чтобы параметры не обходили скраб.
    pub fn texts(&self) -> Vec<String> {
        let mut texts = vec![self.message.id.clone(), self.message.fallback()];
        texts.extend(self.message.context.iter().cloned());
        texts.extend(self.message.plural.iter().cloned());
        texts.extend(self.message.args.keys().cloned());
        texts.extend(self.message.args.values().cloned());
        texts.extend([self.domain.clone(), self.revision.clone()]);
        for (locale, template) in &self.translations {
            texts.push(locale.clone());
            texts.push(template.clone());
            texts.push(interpolate(template, &self.message.args));
        }
        texts
    }
}

#[data]
pub struct Translation {
    pub id: String,
    pub context: Option<String>,
    pub plural: Option<String>,
    pub forms: Vec<String>,
}

#[data]
pub struct Catalog {
    pub language: String,
    pub nplurals: usize,
    pub plural: String,
    pub entries: Vec<Translation>,
}

impl Catalog {
    pub fn parse(source: &str, language: &str) -> Result<Self, String> {
        po::parse(source, language)
    }

    fn lookup(&self, message: &Message) -> Option<&str> {
        let entry = self.entries.iter().find(|entry| {
            entry.id == message.id
                && entry.context == message.context
                && entry.plural == message.plural
        })?;
        let index = if entry.plural.is_some() {
            usize::try_from(plural::evaluate(&self.plural, message.count?).ok()?).ok()?
        } else {
            0
        };
        entry
            .forms
            .get(index)
            .filter(|s| !s.is_empty())
            .map(String::as_str)
    }
}

#[data]
#[derive(Default)]
pub struct Catalogs {
    pub revision: String,
    pub catalogs: BTreeMap<String, Catalog>,
}

impl Catalogs {
    pub fn insert(&mut self, catalog: Catalog) -> Result<(), String> {
        let locale = normalize_locale(&catalog.language);
        if locale.is_empty() || self.catalogs.contains_key(&locale) {
            return Err(format!("empty or duplicate catalog language: {locale:?}"));
        }
        self.catalogs.insert(locale, catalog);
        Ok(())
    }

    pub fn bind(&self, domain: &str, message: Message) -> LocalizedText {
        let translations = self
            .catalogs
            .iter()
            .filter_map(|(locale, catalog)| {
                Some((locale.clone(), catalog.lookup(&message)?.to_owned()))
            })
            .collect();
        LocalizedText {
            domain: domain.into(),
            revision: self.revision.clone(),
            message,
            translations,
        }
    }

    pub fn render(&self, message: Message, locale: &str) -> String {
        self.bind("", message).render(locale)
    }
}

pub fn normalize_locale(locale: &str) -> String {
    locale
        .split(['.', '@'])
        .next()
        .unwrap_or(locale)
        .replace('_', "-")
        .to_ascii_lowercase()
}

fn locale_chain(locale: &str) -> Vec<String> {
    let mut locale = normalize_locale(locale);
    let mut chain = Vec::new();
    loop {
        chain.push(locale.clone());
        let Some((parent, _)) = locale.rsplit_once('-') else {
            break;
        };
        locale = parent.to_owned();
    }
    chain
}

pub fn placeholders(template: &str) -> Result<BTreeSet<String>, String> {
    let mut names = BTreeSet::new();
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
            }
            '{' => {
                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(ch) if ch.is_ascii_alphanumeric() || ch == '_' => name.push(ch),
                        _ => return Err(format!("invalid placeholder in {template:?}")),
                    }
                }
                if name.is_empty() {
                    return Err("empty placeholder".into());
                }
                names.insert(name);
            }
            '}' => return Err(format!("unmatched closing brace in {template:?}")),
            _ => {}
        }
    }
    Ok(names)
}

fn interpolate(template: &str, args: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        if out.len() >= MAX_TEXT {
            break;
        }
        match ch {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut name = String::new();
                for ch in chars.by_ref() {
                    if ch == '}' {
                        break;
                    }
                    name.push(ch);
                    if name.len() >= MAX_TEXT {
                        break;
                    }
                }
                if let Some(value) = args.get(&name) {
                    let remaining = MAX_TEXT.saturating_sub(out.len());
                    let mut end = value.len().min(remaining);
                    while !value.is_char_boundary(end) {
                        end -= 1;
                    }
                    out.push_str(&value[..end]);
                } else {
                    append_bounded(&mut out, "{");
                    append_bounded(&mut out, &name);
                    append_bounded(&mut out, "}");
                }
            }
            _ => {
                if out.len() + ch.len_utf8() > MAX_TEXT {
                    break;
                }
                out.push(ch);
            }
        }
    }
    out
}

fn append_bounded(out: &mut String, value: &str) {
    let mut end = value.len().min(MAX_TEXT.saturating_sub(out.len()));
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    out.push_str(&value[..end]);
}
