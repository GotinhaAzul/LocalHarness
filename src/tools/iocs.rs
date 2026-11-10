//! `iocs` tool: extract and optionally defang indicators of compromise.

use std::collections::HashSet;
use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::tools::{
    Tool, ToolContext, ToolPack, ToolResult, optional_bool, optional_str, optional_u64,
};

/// Hard cap on input text, in characters.
const MAX_INPUT_CHARS: usize = 2_000_000;
/// Default and hard cap on values per category.
const DEFAULT_MAX: usize = 100;
const HARD_MAX: usize = 100;

static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\bhttps?://[^\s"'<>()\[\]{}]+"#).expect("url regex"));

static DOMAIN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,24}\b")
        .expect("domain regex")
});

static IPV4_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:(?:25[0-5]|2[0-4]\d|1?\d?\d)\.){3}(?:25[0-5]|2[0-4]\d|1?\d?\d)\b")
        .expect("ipv4 regex")
});

static IPV6_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:[0-9a-f]{1,4}:){7}[0-9a-f]{1,4}\b|\b(?:[0-9a-f]{1,4}:){1,6}:(?:[0-9a-f]{1,4}(?::[0-9a-f]{1,4}){0,5})?\b|::(?:[0-9a-f]{1,4}(?::[0-9a-f]{1,4}){0,6})\b",
    )
    .expect("ipv6 regex")
});

static SHA256_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[0-9a-fA-F]{64}\b").expect("sha256 regex"));
static SHA1_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[0-9a-fA-F]{40}\b").expect("sha1 regex"));
static MD5_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[0-9a-fA-F]{32}\b").expect("md5 regex"));

static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,24}\b").expect("email regex")
});

static CVE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bCVE-\d{4}-\d+\b").expect("cve regex"));

pub struct IocsTool;

/// One extraction bucket.
struct Category {
    key: &'static str,
    heading: &'static str,
    regex: &'static LazyLock<Regex>,
    defang: Defang,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Defang {
    Url,
    Host,
    Email,
    None,
}

fn categories() -> [Category; 9] {
    [
        Category {
            key: "urls",
            heading: "URLs",
            regex: &URL_RE,
            defang: Defang::Url,
        },
        Category {
            key: "domains",
            heading: "Domains",
            regex: &DOMAIN_RE,
            defang: Defang::Host,
        },
        Category {
            key: "ipv4",
            heading: "IPv4",
            regex: &IPV4_RE,
            defang: Defang::Host,
        },
        Category {
            key: "ipv6",
            heading: "IPv6",
            regex: &IPV6_RE,
            defang: Defang::Host,
        },
        Category {
            key: "sha256",
            heading: "SHA-256",
            regex: &SHA256_RE,
            defang: Defang::None,
        },
        Category {
            key: "sha1",
            heading: "SHA-1",
            regex: &SHA1_RE,
            defang: Defang::None,
        },
        Category {
            key: "md5",
            heading: "MD5",
            regex: &MD5_RE,
            defang: Defang::None,
        },
        Category {
            key: "emails",
            heading: "Emails",
            regex: &EMAIL_RE,
            defang: Defang::Email,
        },
        Category {
            key: "cves",
            heading: "CVEs",
            regex: &CVE_RE,
            defang: Defang::None,
        },
    ]
}

/// File extensions that look like domains but are almost never indicators.
const CODE_EXTENSIONS: [&str; 22] = [
    "rs", "py", "js", "ts", "json", "md", "txt", "toml", "yaml", "yml", "lock", "sh", "c", "h",
    "cpp", "go", "java", "rb", "html", "css", "xml", "ini",
];

impl IocsTool {
    fn extract(category: &Category, text: &str, limit: usize, is_domain: bool) -> Vec<String> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<String> = Vec::new();
        for matched in category.regex.find_iter(text) {
            let mut raw = matched.as_str();
            if category.defang == Defang::Url {
                raw = raw.trim_end_matches(|c: char| {
                    matches!(
                        c,
                        '.' | ',' | ';' | ':' | ')' | ']' | '}' | '>' | '\'' | '"'
                    )
                });
            }
            if raw.is_empty() {
                continue;
            }
            if is_domain && Self::skip_domain(raw, matched.start(), text) {
                continue;
            }
            if seen.insert(raw.to_string()) {
                out.push(raw.to_string());
                if out.len() >= limit {
                    break;
                }
            }
        }
        out
    }

    fn skip_domain(raw: &str, start: usize, text: &str) -> bool {
        if start > 0 && text.as_bytes().get(start.wrapping_sub(1)) == Some(&b'@') {
            return true;
        }
        let tail = raw
            .rsplit('.')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        CODE_EXTENSIONS.contains(&tail.as_str())
    }
}

impl Defang {
    fn apply(self, value: &str) -> String {
        match self {
            Defang::None => value.to_string(),
            Defang::Url => defang_url(value),
            Defang::Host => value.replace('.', "[.]"),
            Defang::Email => value.replace('@', "[@]").replace('.', "[.]"),
        }
    }
}

fn defang_url(url: &str) -> String {
    match url.find("://") {
        Some(index) => {
            let scheme = &url[..index];
            let lowered = scheme.to_ascii_lowercase();
            let scheme = match lowered.as_str() {
                "http" => "hxxp".to_string(),
                "https" => "hxxps".to_string(),
                _ => scheme.to_string(),
            };
            format!("{scheme}{}", url[index..].replace('.', "[.]"))
        }
        None => url.replace('.', "[.]"),
    }
}

#[async_trait]
impl Tool for IocsTool {
    fn name(&self) -> &'static str {
        "iocs"
    }

    fn description(&self) -> &'static str {
        "Extract and defang indicators of compromise."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "text to scan for indicators"},
                "path": {"type": "string", "description": "file to scan when text is absent"},
                "defang": {"type": "boolean", "description": "rewrite indicators as hxxp / [.] (default true)"},
                "max": {"type": "integer", "minimum": 1, "maximum": 100, "description": "values per category (default 100)"}
            },
            "required": [],
            "additionalProperties": false
        })
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Security
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let text = optional_str(&args, "text").filter(|text| !text.trim().is_empty());
        let path = optional_str(&args, "path").filter(|path| !path.trim().is_empty());
        let defang = optional_bool(&args, "defang").unwrap_or(true);
        let max = optional_u64(&args, "max")
            .unwrap_or(DEFAULT_MAX as u64)
            .clamp(1, HARD_MAX as u64) as usize;

        let (source, input) = match (text, path) {
            (Some(text), _) => ("text".to_string(), text),
            (None, Some(path)) => match Self::read_file(ctx, &path) {
                Ok(text) => (path, text),
                Err(error) => return error,
            },
            (None, None) => {
                return ToolResult::err(
                    "IOC_INPUT",
                    "nothing to scan; pass {\"text\":\"...\"} or {\"path\":\"capture.txt\"}",
                );
            }
        };

        let length = input.chars().count();
        if length > MAX_INPUT_CHARS {
            return ToolResult::err(
                "IOC_TOO_LARGE",
                format!(
                    "input has {length} characters; the cap is {MAX_INPUT_CHARS}. Scan a smaller extract."
                ),
            );
        }

        let mut groups: Vec<(&'static str, &'static str, Vec<String>)> = Vec::new();
        let mut counts = Map::new();
        let mut total = 0usize;
        for category in categories() {
            let is_domain = category.key == "domains";
            let mut values = Self::extract(&category, &input, max, is_domain);
            if defang {
                values = values
                    .iter()
                    .map(|value| category.defang.apply(value))
                    .collect();
            }
            counts.insert(category.key.to_string(), json!(values.len()));
            total += values.len();
            groups.push((category.key, category.heading, values));
        }

        let details = json!({
            "counts": counts,
            "total": total,
            "defang": defang,
            "source": source,
        });

        if total == 0 {
            return ToolResult::ok(format!("no indicators found in {source}"))
                .with_details(details);
        }

        let mut content = format!(
            "source: {source}\nmode: {}\n\n",
            if defang { "defanged" } else { "raw" }
        );
        for (_, heading, values) in &groups {
            if values.is_empty() {
                continue;
            }
            content.push_str(&format!("{heading} ({})\n", values.len()));
            for value in values {
                content.push_str("  ");
                content.push_str(value);
                content.push('\n');
            }
            content.push('\n');
        }
        let summary: Vec<String> = groups
            .iter()
            .filter(|(_, _, values)| !values.is_empty())
            .map(|(key, _, values)| format!("{key}={}", values.len()))
            .collect();
        content.push_str(&format!("total: {total} ({})", summary.join(", ")));

        ToolResult::ok(content).with_details(details)
    }
}

impl IocsTool {
    fn read_file(ctx: &ToolContext, path: &str) -> Result<String, ToolResult> {
        let resolved = ctx.resolve(path).map_err(|error| {
            ToolResult::err("IOC_FILE", format!("cannot read {path}: {}", error.content))
        })?;
        let bytes = std::fs::read(&resolved).map_err(|error| {
            ToolResult::err(
                "IOC_FILE",
                format!("cannot read {}: {error}", resolved.display()),
            )
        })?;
        let text = String::from_utf8(bytes).map_err(|_| {
            ToolResult::err(
                "IOC_FILE",
                format!(
                    "{} is not UTF-8 text; binary files are not scanned",
                    resolved.display()
                ),
            )
        })?;
        if text.contains('\0') {
            return Err(ToolResult::err(
                "IOC_FILE",
                format!(
                    "{} looks binary (NUL bytes); extract the text first",
                    resolved.display()
                ),
            ));
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path as FsPath;
    use std::sync::{Arc, Mutex};

    use crate::config::VerifyConfig;
    use crate::paths::Paths;
    use crate::security::SecurityState;
    use crate::session::{Session, SessionData};
    use crate::tools::{BgRegistry, ReadTracker};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    fn context(dir: &FsPath) -> ToolContext {
        let mut paths = Paths::discover(dir).unwrap();
        paths.data_dir = dir.join("data");
        paths.config_dir = dir.join("config");
        paths.ensure_dirs().unwrap();
        let header = SessionData {
            cwd: paths.cwd.display().to_string(),
            endpoint: "test".into(),
            model: "test".into(),
            profile: "test".into(),
            app: "test".into(),
            context: None,
        };
        let session = Session::create(&paths, header, true).unwrap();
        let artifacts = session.artifacts.clone();
        let security = SecurityState::load(&paths, Some("eng".into())).unwrap();
        ToolContext {
            cwd: paths.cwd.clone(),
            path_jail: false,
            session: Arc::new(Mutex::new(session)),
            artifacts,
            cancel: CancellationToken::new(),
            trust: false,
            verify: VerifyConfig::default(),
            profile: "test".into(),
            reads: Arc::new(Mutex::new(ReadTracker::default())),
            bg: Arc::new(Mutex::new(BgRegistry::new())),
            security: Arc::new(Mutex::new(security)),
            paths,
            lsp: Arc::new(crate::tools::lsp::LspService::new()),
            sandbox: None,
        }
    }

    const SAMPLE: &str = "\
reach hxxp://example.test/login
url https://app.test:8443/x?y=1.
contact analyst@corp.test
host evil.test and lib.rs and 10.0.0.5 and 2001:db8::1
hash d41d8cd98f00b204e9800998ecf8427e
sha1 da39a3ee5e6b4b0d3255bfef95601890afd80709
sha256 e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
cve CVE-2024-12345
";

    #[tokio::test]
    async fn extracts_all_categories_and_defangs() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let result = IocsTool.call(&ctx, json!({"text": SAMPLE})).await;
        assert!(!result.is_error, "{}", result.content);
        let counts = result
            .details
            .as_ref()
            .and_then(|details| details.get("counts"))
            .and_then(|counts| counts.as_object())
            .unwrap()
            .clone();
        for key in [
            "urls", "ipv4", "ipv6", "sha256", "sha1", "md5", "emails", "cves",
        ] {
            assert_eq!(counts[key], 1, "category {key} in {}", result.content);
        }
        // example.test (inside the hxxp URL), app.test, and evil.test; the email
        // domain and lib.rs are excluded.
        assert_eq!(counts["domains"], 3, "{}", result.content);
        assert!(result.content.contains("hxxps://app[.]test:8443/x?y=1"));
        assert!(result.content.contains("analyst[@]corp[.]test"));
        assert!(result.content.contains("10[.]0[.]0[.]5"));
        assert!(result.content.contains("CVE-2024-12345"));
        assert!(
            result
                .content
                .contains("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        // code file extension is not a domain
        assert!(!result.content.contains("lib[.]rs"));
    }

    #[tokio::test]
    async fn raw_mode_keeps_indicators() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let result = IocsTool
            .call(&ctx, json!({"text": SAMPLE, "defang": false}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("https://app.test:8443/x?y=1"));
        assert!(result.content.contains("analyst@corp.test"));
        assert!(!result.content.contains("hxxp"));
        assert!(!result.content.contains("[.]"));
    }

    #[tokio::test]
    async fn caps_values_per_category() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let text: String = (0..150)
            .map(|i| format!("https://host{i}.test/path\n"))
            .collect();
        let result = IocsTool.call(&ctx, json!({"text": text, "max": 5})).await;
        assert!(!result.is_error, "{}", result.content);
        let counts = result.details.as_ref().unwrap()["counts"].clone();
        assert_eq!(counts["urls"], 5);
        // hard cap: asking for more than 100 still yields at most 100
        let text: String = (0..150)
            .map(|i| format!("10.0.0.{}\n", (i % 250) + 1))
            .collect();
        let result = IocsTool
            .call(&ctx, json!({"text": text, "max": 9999}))
            .await;
        let counts = result.details.as_ref().unwrap()["counts"].clone();
        assert_eq!(counts["ipv4"], 100);
    }

    #[tokio::test]
    async fn reads_files_and_reports_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        std::fs::write(dir.path().join("capture.txt"), SAMPLE).unwrap();
        let result = IocsTool
            .call(&ctx, json!({"path": "capture.txt", "defang": false}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("source: capture.txt"));

        let missing = IocsTool.call(&ctx, json!({"path": "nope.txt"})).await;
        assert!(missing.is_error);
        assert!(missing.content.contains("IOC_FILE"));
        assert!(missing.content.contains("nope.txt"));

        let neither = IocsTool.call(&ctx, json!({})).await;
        assert!(neither.is_error);
        assert!(neither.content.contains("IOC_INPUT"));

        std::fs::write(dir.path().join("bin.dat"), [0u8, 1, 2, 0, 4]).unwrap();
        let binary = IocsTool.call(&ctx, json!({"path": "bin.dat"})).await;
        assert!(binary.is_error);
        assert!(binary.content.contains("IOC_FILE"));

        std::fs::write(dir.path().join("empty.txt"), "nothing here\n").unwrap();
        let empty = IocsTool.call(&ctx, json!({"path": "empty.txt"})).await;
        assert!(!empty.is_error);
        assert!(empty.content.contains("no indicators found"));
    }

    #[tokio::test]
    async fn oversized_input_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let text = "a".repeat(MAX_INPUT_CHARS + 1);
        let result = IocsTool.call(&ctx, json!({"text": text})).await;
        assert!(result.is_error);
        assert!(result.content.contains("IOC_TOO_LARGE"));
    }
}
