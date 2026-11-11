//! Secret redaction for anything that is persisted or replayed.
//!
//! Session entries, artifacts, evidence, and exported reports pass through
//! [`Redactor`] before they reach disk. Detection is deliberately shape-based:
//! private key blocks, provider token prefixes, bearer/authorization headers,
//! secret-like `.env` assignments, and the exact values of environment
//! variables whose names look secret. Replacements keep the value comparable
//! without exposing it:
//!
//! ```text
//! [REDACTED:<kind>:<first4>...<hash8>]
//! ```
//!
//! File paths and command names are never matched on their own; only values
//! are replaced. The module uses the existing `regex` and `sha2` dependencies.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Captures, Match, Regex};
use sha2::{Digest, Sha256};

/// Environment variable names containing any of these are treated as secret.
const ENV_NAME_MARKERS: &[&str] = &["TOKEN", "SECRET", "KEY", "PASSWORD"];

/// Values shorter than this are not redacted by exact environment match. It
/// would otherwise replace common substrings such as `true` or `dev` in every
/// tool result.
const MIN_ENV_VALUE_CHARS: usize = 8;

/// Regex alternatives, one named group per secret value. The surrounding
/// context (scheme, `NAME=`, `AccountKey=`) stays in the match but outside the
/// named group, so it is preserved verbatim.
const PATTERN: &str = concat!(
    // PEM/OpenSSH private key blocks, including a truncated block at EOF.
    r"(?P<private_key>-----BEGIN[ A-Z0-9]*PRIVATE KEY(?: BLOCK)?-----[\s\S]*?(?:-----END[ A-Z0-9]*PRIVATE KEY(?: BLOCK)?-----|$))",
    r"|(?P<github>\b(?:gh[pousr]_[A-Za-z0-9]{36,255}|github_pat_[A-Za-z0-9_]{20,255})\b)",
    r"|(?P<aws>\b(?:AKIA|ASIA|AIDA|AROA|AIPA|ANPA|ANVA|ASCA|AGPA)[A-Z0-9]{16}\b)",
    r"|(?P<gcp>\b(?:AIza[0-9A-Za-z_-]{35}|GOCSPX-[0-9A-Za-z_-]{20,}|ya29\.[0-9A-Za-z_-]{20,})\b)",
    r"|(?i:(?:\bAccountKey=|SharedAccessKey=|[?&]sig=))(?P<azure_value>[A-Za-z0-9%+/=]{20,})",
    r"|(?i:\bAuthorization\s*:\s*(?:(?:Bearer|Basic|Digest|Token|ApiKey|Key)\s+)?)(?P<auth_value>[A-Za-z0-9\-._~+/]{12,}=*)",
    r"|(?i:\bBearer\s+)(?P<bearer_value>[A-Za-z0-9\-._~+/]{12,}=*)",
    // `.env`, shell, and line-numbered file output. `read` returns lines as
    // `12  NAME=value`, so an optional number, diff marker, and `export` are
    // accepted before the assignment.
    concat!(
        r"|(?m:^[ \t]*(?:[+\-][ \t]*)?(?:\d+[ \t]+)?(?:export[ \t]+)?",
        r"(?P<env_name>[A-Za-z_][A-Za-z0-9_]*(?i:TOKEN|SECRET|KEY|PASSWORD|PASSWD|CREDENTIAL)[A-Za-z0-9_]*)",
        r"[ \t]*=[ \t]*)",
        r#"(?P<env_value>"[^"\r\n]*"|'[^'\r\n]*'|[^\s#\r\n]+)"#,
    ),
    // `.env` files also carry connection strings and opaque tokens under names
    // that do not contain a marker word. Redact only values that look
    // sensitive; paths and ordinary settings stay readable.
    concat!(
        r"|(?m:^[ \t]*(?:[+\-][ \t]*)?(?:\d+[ \t]+)?(?:export[ \t]+)?",
        r"(?P<env_generic_name>[A-Za-z_][A-Za-z0-9_]*)[ \t]*=[ \t]*)",
        r#"(?P<env_generic_value>"[^"\r\n]*"|'[^'\r\n]*'|[^\s#;\r\n]+)"#,
    ),
);

static PATTERNS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(PATTERN).expect("redaction pattern compiles"));

const GROUPS: &[(&str, &str)] = &[
    ("private_key", "private_key"),
    ("github", "github"),
    ("aws", "aws"),
    ("gcp", "gcp"),
    ("azure_value", "azure"),
    ("auth_value", "auth_header"),
    ("bearer_value", "bearer"),
    ("env_value", "env"),
    ("env_generic_value", "env_generic"),
    ("env_exact", "env"),
];

/// Redacts secret values in text. Cloning is cheap after construction; the
/// environment snapshot is compiled once per instance.
#[derive(Debug, Clone)]
pub struct Redactor {
    enabled: bool,
    /// Static patterns plus the exact values of secret-like environment
    /// variables, compiled once per instance.
    patterns: Option<Regex>,
}

impl Redactor {
    /// Build a redactor from the current process environment. With
    /// `enabled = false` [`Redactor::redact`] returns the input untouched.
    pub fn new(enabled: bool) -> Self {
        if !enabled {
            return Self {
                enabled: false,
                patterns: None,
            };
        }
        // `vars()` panics on non-Unicode entries; a harness must not.
        let values = std::env::vars_os().filter_map(|(name, value)| {
            let name = name.into_string().ok()?;
            let value = value.into_string().ok()?;
            (env_name_is_secret(&name) && env_value_is_usable(&value)).then_some(value)
        });
        Self::from_values(values)
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Redact every detected value. Returns a borrow when nothing matched.
    pub fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let Some(patterns) = &self.patterns else {
            return Cow::Borrowed(text);
        };
        if text.is_empty() {
            return Cow::Borrowed(text);
        }

        let mut matches = patterns.captures_iter(text).peekable();
        if matches.peek().is_none() {
            return Cow::Borrowed(text);
        }

        let mut replaced = String::with_capacity(text.len());
        let mut last = 0usize;
        let mut changed = false;
        for caps in matches {
            let Some(whole) = caps.get(0) else {
                continue;
            };
            replaced.push_str(&text[last..whole.start()]);
            let Some((raw_kind, value)) = classify(&caps) else {
                replaced.push_str(whole.as_str());
                last = whole.end();
                continue;
            };
            if should_redact(raw_kind, value.as_str(), whole.as_str()) {
                let kind = refine_kind(raw_kind, value.as_str());
                changed = true;
                replaced.push_str(&text[whole.start()..value.start()]);
                replaced.push_str(&placeholder(kind, value.as_str()));
                replaced.push_str(&text[value.end()..whole.end()]);
            } else {
                replaced.push_str(whole.as_str());
            }
            last = whole.end();
        }
        if !changed {
            return Cow::Borrowed(text);
        }
        replaced.push_str(&text[last..]);
        Cow::Owned(replaced)
    }

    /// Convenience wrapper for the common persistence path.
    pub fn redact_owned(&self, text: &str) -> String {
        match self.redact(text) {
            Cow::Borrowed(_) => text.to_string(),
            Cow::Owned(redacted) => redacted,
        }
    }

    /// Redact every string in a JSON value, leaving object keys untouched.
    pub fn redact_json(&self, value: &mut serde_json::Value) {
        if !self.enabled {
            return;
        }
        match value {
            serde_json::Value::String(text) => *text = self.redact_owned(text),
            serde_json::Value::Array(items) => {
                for item in items {
                    self.redact_json(item);
                }
            }
            serde_json::Value::Object(map) => {
                for item in map.values_mut() {
                    self.redact_json(item);
                }
            }
            _ => {}
        }
    }

    /// Build a redactor from an explicit value list. Environment names are
    /// checked here so tests can exercise the same filtering as production.
    fn from_values<I>(values: I) -> Self
    where
        I: IntoIterator<Item = String>,
    {
        let mut values: Vec<String> = values
            .into_iter()
            .filter(|value| env_value_is_usable(value))
            .collect();
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        values.dedup();
        values.truncate(256);

        let patterns = if values.is_empty() {
            PATTERNS.clone()
        } else {
            let alternation = values
                .iter()
                .map(|value| regex::escape(value))
                .collect::<Vec<_>>()
                .join("|");
            Regex::new(&format!("{PATTERN}|(?P<env_exact>{alternation})"))
                .unwrap_or_else(|_| PATTERNS.clone())
        };
        Self {
            enabled: true,
            patterns: Some(patterns),
        }
    }

    #[cfg(test)]
    fn with_env_values(values: &[(&str, &str)]) -> Self {
        let values = values
            .iter()
            .filter(|(name, _)| env_name_is_secret(name))
            .map(|(_, value)| (*value).to_string());
        Self::from_values(values)
    }
}

fn env_name_is_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ENV_NAME_MARKERS.iter().any(|marker| upper.contains(marker))
}

fn env_value_is_usable(value: &str) -> bool {
    value.chars().count() >= MIN_ENV_VALUE_CHARS
        && value.len() <= 1024
        && !value.contains('\n')
        && !value.contains('\r')
        && !is_placeholder(value)
}

fn classify<'h>(caps: &Captures<'h>) -> Option<(&'static str, Match<'h>)> {
    GROUPS
        .iter()
        .find_map(|(group, kind)| caps.name(group).map(|found| (*kind, found)))
}

/// An assignment can contain a provider token. Keep the more specific kind
/// (for example `aws`) so evidence placeholders stay descriptive.
fn refine_kind(kind: &str, value: &str) -> &'static str {
    if kind != "env" {
        return match kind {
            "private_key" => "private_key",
            "github" => "github",
            "aws" => "aws",
            "gcp" => "gcp",
            "azure" => "azure",
            "auth_header" => "auth_header",
            "bearer" => "bearer",
            _ => "env",
        };
    }
    let value = value.trim_matches(|ch| ch == '"' || ch == '\'');
    let upper = value.to_ascii_uppercase();
    if upper.len() == 20
        && [
            "AKIA", "ASIA", "AIDA", "AROA", "AIPA", "ANPA", "ANVA", "ASCA", "AGPA",
        ]
        .iter()
        .any(|prefix| upper.starts_with(prefix))
        && upper.chars().all(|ch| ch.is_ascii_alphanumeric())
    {
        "aws"
    } else if value.starts_with("ghp_")
        || value.starts_with("gho_")
        || value.starts_with("ghu_")
        || value.starts_with("ghs_")
        || value.starts_with("ghr_")
        || value.starts_with("github_pat_")
    {
        "github"
    } else if value.starts_with("AIza") || value.starts_with("GOCSPX-") {
        "gcp"
    } else if value.starts_with("AccountKey=") {
        "azure"
    } else {
        "env"
    }
}

fn should_redact(kind: &str, value: &str, whole: &str) -> bool {
    if value.is_empty() || is_placeholder(value) {
        return false;
    }
    match kind {
        "auth_header" | "bearer" => {
            if whole.to_ascii_lowercase().contains("bearer ") {
                // A bearer scheme is already an explicit credential signal.
                // Long values are tokens; short ones still need entropy so
                // prose such as "Bearer authentication" stays readable.
                value.chars().count() >= 20 || looks_high_entropy(value)
            } else {
                looks_high_entropy(value)
            }
        }
        "env_generic" => looks_sensitive_assignment(value),
        _ => true,
    }
}

/// A generic `.env` value is redacted only when it looks like a credential or
/// a URL with userinfo. Plain paths (which are never redacted) and ordinary
/// settings such as `PORT=8080` pass through.
fn looks_sensitive_assignment(value: &str) -> bool {
    let value = value.trim_matches(|ch| ch == '"' || ch == '\'');
    if looks_like_path(value) {
        return false;
    }
    if let Some(scheme) = value.find("://") {
        // Ordinary URLs are not secrets; userinfo in a URL is.
        return value[scheme + 3..].contains('@');
    }
    looks_high_entropy(value)
}

fn looks_like_path(value: &str) -> bool {
    if value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with('~')
        || value.starts_with("C:\\")
        || value.starts_with("c:\\")
    {
        return true;
    }
    !value.contains("://") && value.contains('/') && !value.contains('@')
}

/// Heuristic for opaque authorization values. `authentication` and prose do
/// not qualify; base64/hex tokens with a few character classes do.
fn looks_high_entropy(value: &str) -> bool {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() < 8 {
        return false;
    }
    let (mut lower, mut upper, mut digit, mut symbol) = (false, false, false, false);
    for ch in &chars {
        if ch.is_ascii_lowercase() {
            lower = true;
        } else if ch.is_ascii_uppercase() {
            upper = true;
        } else if ch.is_ascii_digit() {
            digit = true;
        } else {
            symbol = true;
        }
    }
    let classes = [lower, upper, digit, symbol]
        .into_iter()
        .filter(|present| *present)
        .count();
    classes >= 2 && shannon_entropy(&chars) >= 3.0
}

fn shannon_entropy(chars: &[char]) -> f64 {
    use std::collections::HashMap;
    let mut counts: HashMap<char, usize> = HashMap::new();
    for ch in chars {
        *counts.entry(*ch).or_insert(0) += 1;
    }
    let len = chars.len() as f64;
    counts.values().fold(0.0, |entropy, count| {
        let probability = *count as f64 / len;
        entropy - probability * probability.log2()
    })
}

fn is_placeholder(value: &str) -> bool {
    let trimmed = value.trim_matches(|ch| ch == '"' || ch == '\'' || ch == ' ' || ch == '\t');
    trimmed.starts_with("[REDACTED:")
}

/// Build `[REDACTED:<kind>:<first4>...<hash8>]`. The first four characters
/// are omitted for short values so the prefix cannot reveal most of a secret.
fn placeholder(kind: &str, value: &str) -> String {
    let prefix: String = if value.chars().count() >= 12 {
        value.chars().take(4).collect()
    } else {
        String::new()
    };
    let digest = Sha256::digest(value.as_bytes());
    let hash = format!("{digest:x}");
    format!(
        "[REDACTED:{kind}:{prefix}...{}]",
        hash.get(..8).unwrap_or(&hash)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const AWS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
    const GITHUB_TOKEN: &str = "ghp_0123456789abcdefghijklmnopqrstuvwxyz";
    const GCP_KEY: &str = "AIza0123456789abcdefghijklmnopqrstuvwxy";

    fn redactor() -> Redactor {
        Redactor::with_env_values(&[("LH_TEST_TOKEN", "env-secret-value-123")])
    }

    #[test]
    fn redacts_aws_keys_and_keeps_context() {
        let text = format!("aws_access_key_id = {AWS_KEY}\n");
        let redacted = redactor().redact_owned(&text);
        assert!(!redacted.contains(AWS_KEY), "{redacted}");
        assert!(redacted.contains("aws_access_key_id = "), "{redacted}");
        assert!(redacted.contains("[REDACTED:aws:AKIA..."), "{redacted}");
        assert!(redacted.contains("..."), "{redacted}");
    }

    #[test]
    fn redacts_github_and_gcp_tokens() {
        let redacted =
            redactor().redact_owned(&format!("remote: {GITHUB_TOKEN}\nkey: {GCP_KEY}\n"));
        assert!(!redacted.contains(GITHUB_TOKEN), "{redacted}");
        assert!(!redacted.contains(GCP_KEY), "{redacted}");
        assert!(redacted.contains("[REDACTED:github:ghp_..."), "{redacted}");
        assert!(redacted.contains("[REDACTED:gcp:AIza..."), "{redacted}");
    }

    #[test]
    fn redacts_azure_connection_string_values() {
        let text = "DefaultEndpointsProtocol=https;AccountName=demo;AccountKey=c3VwZXJzZWNyZXRrZXl2YWx1ZTEyMzQ1Njc4OTA=\n";
        let redacted = redactor().redact_owned(text);
        assert!(
            !redacted.contains("c3VwZXJzZWNyZXRrZXl2YWx1ZTEyMzQ1Njc4OTA"),
            "{redacted}"
        );
        assert!(redacted.contains("AccountKey="), "{redacted}");
        assert!(redacted.contains("[REDACTED:azure:"), "{redacted}");
    }

    #[test]
    fn redacts_bearer_headers_and_keeps_scheme_and_command() {
        let token = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.signature";
        let text = format!("curl -H \"Authorization: Bearer {token}\" https://example.test\n");
        let redacted = redactor().redact_owned(&text);
        assert!(!redacted.contains(token), "{redacted}");
        assert!(
            redacted.contains("curl -H \"Authorization: Bearer "),
            "{redacted}"
        );
        assert!(redacted.contains("[REDACTED:auth_header:"), "{redacted}");
    }

    #[test]
    fn redacts_high_entropy_basic_authorization_values() {
        let text = "Authorization: Basic dXNlcjpwYXNzd29yZA==\n";
        let redacted = redactor().redact_owned(text);
        assert!(!redacted.contains("dXNlcjpwYXNzd29yZA=="), "{redacted}");
        assert!(
            redacted.contains("Authorization: Basic [REDACTED:auth_header:"),
            "{redacted}"
        );
    }

    #[test]
    fn redacts_private_key_blocks() {
        let text = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----\nafter\n";
        let redacted = redactor().redact_owned(text);
        assert!(!redacted.contains("b3BlbnNzaC1rZXktdjEAAAAA"), "{redacted}");
        assert!(
            redacted.contains("[REDACTED:private_key:----..."),
            "{redacted}"
        );
        assert!(redacted.contains("after"), "{redacted}");

        let pgp =
            "-----BEGIN PGP PRIVATE KEY BLOCK-----\nbody\n-----END PGP PRIVATE KEY BLOCK-----\n";
        let redacted = redactor().redact_owned(pgp);
        assert!(!redacted.contains("body"), "{redacted}");
        assert!(redacted.contains("[REDACTED:private_key:"), "{redacted}");
    }

    #[test]
    fn env_assignments_keep_names_and_skip_ordinary_values() {
        let text = "1  DB_PASSWORD=hunter2\n2  GITHUB_TOKEN=abc123456789\n3  PORT=8080\n4  export API_KEY=\"quoted-secret\"\n5  DATABASE_URL=postgres://user:pass@db.test/app\n6  HOME=/home/user\n7  PACKAGE=src/lib.rs\n8  RUST_LOG=debug\n9  API_BASE=https://api.example.test/v1\n";
        let redacted = redactor().redact_owned(text);
        assert!(!redacted.contains("hunter2"), "{redacted}");
        assert!(!redacted.contains("abc123456789"), "{redacted}");
        assert!(!redacted.contains("quoted-secret"), "{redacted}");
        assert!(!redacted.contains("pass@db.test"), "{redacted}");
        assert!(
            redacted.contains("DB_PASSWORD=[REDACTED:env:"),
            "{redacted}"
        );
        assert!(
            redacted.contains("GITHUB_TOKEN=[REDACTED:env:"),
            "{redacted}"
        );
        assert!(
            redacted.contains("DATABASE_URL=[REDACTED:env:"),
            "{redacted}"
        );
        assert!(redacted.contains("PORT=8080"), "{redacted}");
        assert!(redacted.contains("HOME=/home/user"), "{redacted}");
        assert!(redacted.contains("PACKAGE=src/lib.rs"), "{redacted}");
        assert!(redacted.contains("RUST_LOG=debug"), "{redacted}");
        assert!(
            redacted.contains("API_BASE=https://api.example.test/v1"),
            "{redacted}"
        );
        assert!(redacted.contains("export API_KEY="), "{redacted}");
    }

    #[test]
    fn redacts_exact_environment_values() {
        let redacted = redactor().redact_owned("the value is env-secret-value-123 here");
        assert!(!redacted.contains("env-secret-value-123"), "{redacted}");
        assert!(redacted.contains("[REDACTED:env:env-..."), "{redacted}");
    }

    #[test]
    fn leaves_paths_commands_and_ordinary_text_alone() {
        let text =
            "nmap example.test\n/home/user/.ssh/id_rsa\nAuthorization: Bearer authentication\n";
        let redacted = redactor().redact_owned(text);
        assert_eq!(redacted, text);
    }

    #[test]
    fn disabled_returns_the_input_untouched() {
        let redactor = Redactor::new(false);
        let text = format!("{AWS_KEY}\n");
        assert!(matches!(redactor.redact(&text), Cow::Borrowed(_)));
        assert_eq!(redactor.redact_owned(&text), text);
    }

    #[test]
    fn redaction_is_idempotent() {
        let text = format!("key = {AWS_KEY}\nDB_PASSWORD=hunter2\n");
        let once = redactor().redact_owned(&text);
        let twice = redactor().redact_owned(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn same_value_has_same_placeholder_and_different_values_differ() {
        let redactor = redactor();
        let one = redactor.redact_owned(&format!("{AWS_KEY} and {AWS_KEY}"));
        assert_eq!(one.matches("[REDACTED:aws:").count(), 2);
        let left = one.split(" and ").next().unwrap().to_string();
        let right = one.split(" and ").nth(1).unwrap().to_string();
        assert_eq!(left, right);
        let other = redactor.redact_owned("AKIAZZZZZZZZZZZZZZZZ");
        assert_ne!(left, other);
    }

    #[test]
    fn short_values_do_not_leak_a_prefix() {
        let redactor = Redactor::with_env_values(&[("LH_PASSWORD", "abcdefgh")]);
        let redacted = redactor.redact_owned("abcdefgh");
        assert!(redacted.starts_with("[REDACTED:env:..."), "{redacted}");
    }

    #[test]
    fn redaction_stays_under_five_milliseconds() {
        let redactor = Redactor::new(true);
        let text = "PORT 443 open\nserver: nginx\n".repeat(2048);
        // Warm the regex so the measurement covers redaction, not compilation.
        let _ = redactor.redact_owned(&text);
        let started = std::time::Instant::now();
        let iterations = 20;
        for _ in 0..iterations {
            let redacted = redactor.redact_owned(&text);
            assert!(!redacted.is_empty());
        }
        let per_result = started.elapsed() / iterations;
        assert!(
            per_result < std::time::Duration::from_millis(5),
            "redaction took {per_result:?} per 61 KB result"
        );
    }
}
