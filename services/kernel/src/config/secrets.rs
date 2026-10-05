//! Секция `secrets` (`05 §9`): правила скраба — данные, не код. Свой список
//! замещает дефолт целиком, `[]` — скраба нет (I8); битый regex — отказ старта.

use eva::data;

#[data]
#[serde(default, deny_unknown_fields)]
pub struct SecretsConfig {
    /// По порядку; синтаксис — крейт `regex` (без lookaround).
    pub rules: Vec<SecretRule>,
}

#[data]
pub struct SecretRule {
    pub name: String,
    pub pattern: String,
    /// `$1`… — группы; плейсхолдер `[redacted:<name>]` пишется в `replace` явно.
    pub replace: String,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        let rule = |name: &str, pattern: &str, replace: &str| SecretRule {
            name: name.to_owned(),
            pattern: pattern.to_owned(),
            replace: replace.to_owned(),
        };
        Self {
            rules: vec![
                rule(
                    "bearer",
                    r"(?i)bearer\s+[A-Za-z0-9._~+/=-]{16,}",
                    "Bearer [redacted:bearer]",
                ),
                rule("sk_key", r"sk-[A-Za-z0-9_-]{20,}", "[redacted:sk_key]"),
                rule("aws_key", r"AKIA[0-9A-Z]{16}", "[redacted:aws_key]"),
                rule(
                    "age_key",
                    r"AGE-SECRET-KEY-1[0-9A-Z]{58}",
                    "[redacted:age_key]",
                ),
                rule(
                    "gitlab_pat",
                    r"glpat-[A-Za-z0-9_-]{20,}",
                    "[redacted:gitlab_pat]",
                ),
                rule(
                    "github_token",
                    r"gh[pousr]_[A-Za-z0-9]{36,}",
                    "[redacted:github_token]",
                ),
                rule(
                    "pem",
                    r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
                    "[redacted:pem]",
                ),
                rule(
                    "assignment",
                    r#"(?i)\b(api[_-]?key|token|secret|password|passwd)\b(["']?)\s*[:=]\s*(["']?)[^\s"'\[][^\s"']{7,}"#,
                    "$1$2$3[redacted:assignment]",
                ),
            ],
        }
    }
}
