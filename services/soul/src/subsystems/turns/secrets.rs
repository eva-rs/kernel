//! Скраб секретов (`05 §9`): одна точка применения — результат тула после
//! хуков `after_tool` и до окна, дедупа и лога; тем же скрабом — `progress`
//! тулов и текст fatal-ошибок тура.

use std::borrow::Cow;

use regex::Regex;

use crate::config::secrets::SecretsConfig;

pub struct Scrubber {
    rules: Vec<(String, Regex, String)>,
}

impl std::fmt::Debug for Scrubber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scrubber")
            .field(
                "rules",
                &self.rules.iter().map(|r| &r.0).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Scrubber {
    pub fn progress(
        &self,
        text: String,
        message: Option<Box<eva_protocol::types::text::LocalizedText>>,
    ) -> (
        String,
        Option<Box<eva_protocol::types::text::LocalizedText>>,
    ) {
        let message = message.filter(|message| message.valid());
        let text = message
            .as_ref()
            .map_or(text, |message| message.message.fallback());
        let (text, hits) = self.scrub(&text);
        let message = message.filter(|message| {
            hits == 0 && message.texts().iter().all(|text| self.scrub(text).1 == 0)
        });
        (text.into_owned(), message)
    }

    /// Битый regex — ошибка загрузки конфига с именем правила.
    pub fn new(cfg: &SecretsConfig) -> eyre::Result<Self> {
        let rules = cfg
            .rules
            .iter()
            .map(|r| {
                Regex::new(&r.pattern)
                    .map(|re| (r.name.clone(), re, r.replace.clone()))
                    .map_err(|e| eyre::eyre!("secrets.rules {}: invalid pattern: {e}", r.name))
            })
            .collect::<eyre::Result<_>>()?;
        Ok(Self { rules })
    }

    /// Текст после всех правил по порядку и число замен.
    pub fn scrub<'a>(&self, text: &'a str) -> (Cow<'a, str>, u32) {
        let mut out = Cow::Borrowed(text);
        let mut hits = 0_u32;
        for (_, re, replace) in &self.rules {
            let n = re.find_iter(&out).count();
            if n == 0 {
                continue;
            }
            hits += u32::try_from(n).unwrap_or(u32::MAX);
            out = Cow::Owned(re.replace_all(&out, replace.as_str()).into_owned());
        }
        (out, hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::secrets::SecretRule;

    #[test]
    fn localized_progress_cannot_reconstruct_redacted_secrets() {
        use eva_protocol::types::text::{Catalogs, Message};
        let scrubber = Scrubber::new(&SecretsConfig::default()).unwrap();
        let mut message = Catalogs::default().bind(
            "echo",
            Message::new("token {value}").arg("value", "sk-ABCDEFGHIJKLMNOPQRSTUV"),
        );
        message
            .translations
            .insert("ru".into(), "ключ {value}".into());
        let (text, localized) = scrubber.progress("ignored".into(), Some(Box::new(message)));
        assert!(!text.contains("ABCDEFGHIJKLMNOP"));
        assert!(localized.is_none());

        let mut message = Catalogs::default().bind("echo", Message::new("safe"));
        message
            .translations
            .insert("ru".into(), "Bearer abcdefghijklmnop1234".into());
        let (text, localized) = scrubber.progress("safe".into(), Some(Box::new(message)));
        assert_eq!(text, "safe");
        assert!(localized.is_none());
    }

    #[test]
    fn default_rules_redact_and_are_idempotent() {
        let s = Scrubber::new(&SecretsConfig::default()).expect("default rules compile");
        let text = "auth: Bearer abcdefghijklmnop1234\nkey sk-ABCDEFGHIJKLMNOPQRSTUV\napi_key=supersecretvalue";
        let (once, hits) = s.scrub(text);
        assert!(hits >= 3, "{once}");
        assert!(once.contains("Bearer [redacted:bearer]"), "{once}");
        assert!(once.contains("[redacted:sk_key]"), "{once}");
        assert!(once.contains("api_key[redacted:assignment]"), "{once}");
        assert!(!once.contains("supersecretvalue"));
        let (twice, again) = s.scrub(&once);
        assert_eq!(twice, once);
        assert_eq!(again, 0);
        let (plain, none) = s.scrub("nothing to see");
        assert!(matches!(plain, Cow::Borrowed(_)));
        assert_eq!(none, 0);
    }

    #[test]
    fn empty_list_disables_and_broken_regex_refuses() {
        let off = Scrubber::new(&SecretsConfig { rules: Vec::new() }).expect("empty");
        assert_eq!(off.scrub("sk-ABCDEFGHIJKLMNOPQRSTUV").1, 0);
        let broken = SecretsConfig {
            rules: vec![SecretRule {
                name: "bad".into(),
                pattern: "(".into(),
                replace: "x".into(),
            }],
        };
        let err = Scrubber::new(&broken).expect_err("broken");
        assert!(err.to_string().contains("secrets.rules bad"), "{err}");
    }
}
