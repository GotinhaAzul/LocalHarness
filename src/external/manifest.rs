//! External tool manifests: TOML parsing, validation, and schema loading.
//!
//! A manifest is user-authored configuration; a broken one is skipped with a
//! warning rather than taking the whole external tool set down. Validation
//! happens once at load time so the call path only deals with usable tools.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Hard ceiling for a single external call. Long scans belong in `bg`.
pub const MAX_TIMEOUT_SECS: u64 = 3600;
pub const MAX_NAME_LEN: usize = 64;

/// One external tool manifest file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    pub description: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub input_schema: String,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub shell: bool,
    #[serde(default)]
    pub scope_gate: bool,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

impl Manifest {
    /// Reject manifests that cannot produce a usable tool. Messages name the
    /// exact field to fix.
    pub fn validate(&self) -> Result<()> {
        let name = self.name.trim();
        if name.is_empty() {
            bail!("`name` is empty; give the tool a name like \"semgrep\"");
        }
        if name != self.name {
            bail!("`name` {:?} has leading or trailing whitespace", self.name);
        }
        if name.len() > MAX_NAME_LEN {
            bail!("`name` {name:?} is longer than {MAX_NAME_LEN} characters");
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            bail!("`name` {name:?} must contain only ASCII letters, digits, '_' or '-'");
        }
        if self.description.trim().is_empty() {
            bail!("`description` is empty; write one line describing what the tool returns");
        }
        if self.command.trim().is_empty() {
            bail!("`command` is empty; name the executable to run, for example \"semgrep\"");
        }
        if self.command.contains('\0') {
            bail!("`command` contains a NUL byte");
        }
        if self.timeout_secs == 0 {
            bail!("`timeout_secs` must be at least 1; use a short timeout and `bg` for long scans");
        }
        for (key, value) in &self.env {
            if key.is_empty() || key.contains('=') || key.contains('\0') {
                bail!("`env` key {key:?} is invalid; use plain names like SEMGREP_SEND_METRICS");
            }
            if value.contains('\0') {
                bail!("`env` value for {key:?} contains a NUL byte");
            }
        }
        Ok(())
    }

    /// Timeout for one call, clamped to the hard ceiling.
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.clamp(1, MAX_TIMEOUT_SECS))
    }
}

/// A manifest plus its resolved JSON input schema.
#[derive(Debug, Clone)]
pub struct LoadedManifest {
    pub manifest: Manifest,
    pub schema: Value,
    pub manifest_dir: PathBuf,
}

/// Read, parse, and validate one manifest file and its schema.
pub fn load_file(path: &Path) -> Result<LoadedManifest> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("cannot read manifest {}", path.display()))?;
    let manifest: Manifest = toml::from_str(&text)
        .with_context(|| format!("cannot parse {} as TOML", path.display()))?;
    manifest
        .validate()
        .with_context(|| format!("invalid manifest {}", path.display()))?;

    let manifest_dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let schema_path = resolve_schema_path(&manifest_dir, &manifest.input_schema)?;
    let schema_text = fs::read_to_string(&schema_path).with_context(|| {
        format!(
            "cannot read input_schema {} (referenced by {})",
            schema_path.display(),
            path.display()
        )
    })?;
    let schema: Value = serde_json::from_str(&schema_text)
        .with_context(|| format!("input_schema {} is not valid JSON", schema_path.display()))?;
    validate_schema(&schema).with_context(|| {
        format!(
            "input_schema {} is not a usable object schema",
            schema_path.display()
        )
    })?;

    Ok(LoadedManifest {
        manifest,
        schema,
        manifest_dir,
    })
}

fn resolve_schema_path(manifest_dir: &Path, input_schema: &str) -> Result<PathBuf> {
    if input_schema.trim().is_empty() {
        bail!("`input_schema` is empty; point it at a JSON schema file next to the manifest");
    }
    let raw = Path::new(input_schema);
    if raw.is_absolute() {
        bail!(
            "`input_schema` must be relative to the manifest, got {}",
            raw.display()
        );
    }
    let base = crate::tools::normalize(manifest_dir);
    let joined = crate::tools::normalize(&base.join(raw));
    if !joined.starts_with(&base) {
        bail!(
            "`input_schema` {} escapes the manifest directory {}",
            raw.display(),
            manifest_dir.display()
        );
    }
    Ok(joined)
}

/// The harness sends one flat object of arguments; the schema must describe
/// exactly that shape.
pub fn validate_schema(schema: &Value) -> Result<()> {
    let object = schema
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("`input_schema` must be a JSON object"))?;
    match object.get("type") {
        Some(Value::String(kind)) if kind == "object" => {}
        Some(_) | None => bail!("`input_schema` must declare {{\"type\": \"object\"}}"),
    }
    if let Some(properties) = object.get("properties")
        && !properties.is_object()
    {
        bail!("`properties` in `input_schema` must be an object");
    }
    if let Some(required) = object.get("required") {
        let list = required
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("`required` in `input_schema` must be an array"))?;
        if list.iter().any(|item| !item.is_string()) {
            bail!("`required` in `input_schema` must contain only strings");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
name = "semgrep"
description = "Run semgrep and return JSON findings"
command = "semgrep"
args = ["--json", "--config", "auto", "{target}"]
input_schema = "semgrep.schema.json"
timeout_secs = 120
shell = false
scope_gate = false

[env]
SEMGREP_SEND_METRICS = "off"
"#;

    const SCHEMA: &str =
        r#"{"type":"object","properties":{"target":{"type":"string"}},"required":["target"]}"#;

    #[test]
    fn parses_the_documented_example() {
        let manifest: Manifest = toml::from_str(EXAMPLE).unwrap();
        manifest.validate().unwrap();
        assert_eq!(manifest.name, "semgrep");
        assert_eq!(manifest.command, "semgrep");
        assert_eq!(
            manifest.args,
            vec!["--json", "--config", "auto", "{target}"]
        );
        assert_eq!(manifest.timeout(), Duration::from_secs(120));
        assert!(!manifest.shell);
        assert!(!manifest.scope_gate);
        assert_eq!(
            manifest.env.get("SEMGREP_SEND_METRICS").map(String::as_str),
            Some("off")
        );
    }

    #[test]
    fn optional_fields_default() {
        let text = r#"
name = "demo"
description = "demo tool"
command = "demo"
input_schema = "demo.schema.json"
"#;
        let manifest: Manifest = toml::from_str(text).unwrap();
        manifest.validate().unwrap();
        assert_eq!(manifest.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert!(manifest.args.is_empty());
        assert!(!manifest.shell);
        assert!(!manifest.scope_gate);
        assert!(manifest.env.is_empty());
    }

    #[test]
    fn rejects_invalid_names_and_timeouts() {
        let mut manifest: Manifest = toml::from_str(EXAMPLE).unwrap();
        manifest.name = "bad name".into();
        assert!(manifest.validate().is_err());

        manifest.name = "semgrep".into();
        manifest.timeout_secs = 0;
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn loads_schema_relative_to_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("semgrep.schema.json"), SCHEMA).unwrap();
        fs::write(dir.path().join("semgrep.toml"), EXAMPLE).unwrap();

        let loaded = load_file(&dir.path().join("semgrep.toml")).unwrap();
        assert_eq!(loaded.manifest.name, "semgrep");
        assert_eq!(loaded.schema["type"], "object");
        assert_eq!(loaded.manifest_dir, dir.path());
    }

    #[test]
    fn missing_schema_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("semgrep.toml"), EXAMPLE).unwrap();
        assert!(load_file(&dir.path().join("semgrep.toml")).is_err());
    }

    #[test]
    fn schema_must_be_an_object_schema() {
        assert!(validate_schema(&serde_json::json!({"type": "array"})).is_err());
        assert!(validate_schema(&serde_json::json!({"type": "object"})).is_ok());
    }

    #[test]
    fn schema_path_cannot_escape_the_manifest_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_schema_path(dir.path(), "../outside.json").is_err());
        assert!(resolve_schema_path(dir.path(), "/etc/passwd").is_err());
    }
}
