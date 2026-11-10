//! External tools: manifest discovery and the stdin/stdout protocol.
//!
//! An external tool is any executable with a TOML manifest and a JSON schema.
//! The harness passes one JSON request on stdin and accepts either one JSON
//! result object or plain text on stdout. External tools are never part of a
//! default pack: `load_tools` returns them explicitly so `ToolRegistry::select`
//! can attach them to a profile or workflow.

pub mod manifest;
pub mod runner;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::paths::Paths;
use crate::tools::{Tool, ToolContext, ToolPack, ToolResult};

use manifest::{LoadedManifest, Manifest};
use runner::{PreparedCall, RunError, RunOutput};

/// Result content cap. The agent caps again per batch; this keeps one tool from
/// flooding it.
pub const CONTENT_CAP: usize = 64 * 1024;
/// Stderr is quoted into error messages at this length.
pub const STDERR_MESSAGE_CAP: usize = 1000;
/// Protocol `details` above this size are dropped instead of passed through.
pub const DETAILS_CAP: usize = 32 * 1024;

/// Discover external tools. Global manifests are trusted; project manifests
/// (`<cwd>/.harness/tools/*.toml`) load only when `trust` is set. A project
/// manifest overrides a global one with the same `name`. Broken manifests are
/// logged and skipped.
pub fn load_tools(paths: &Paths, trust: bool) -> Result<Vec<Arc<dyn Tool>>> {
    let mut discovered: BTreeMap<String, LoadedManifest> = BTreeMap::new();
    collect_manifests(&paths.tools_dir(), &mut discovered);
    if trust {
        collect_manifests(&paths.project_dir().join("tools"), &mut discovered);
    }

    let mut tools: Vec<Arc<dyn Tool>> = Vec::with_capacity(discovered.len());
    for loaded in discovered.into_values() {
        tools.push(Arc::new(ExternalTool::new(loaded)) as Arc<dyn Tool>);
    }
    Ok(tools)
}

fn collect_manifests(dir: &Path, into: &mut BTreeMap<String, LoadedManifest>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(
                dir = %dir.display(),
                %error,
                "external tool directory could not be read"
            );
            return;
        }
    };

    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("toml"))
        .collect();
    files.sort();

    for file in files {
        match manifest::load_file(&file) {
            Ok(loaded) => {
                into.insert(loaded.manifest.name.clone(), loaded);
            }
            Err(error) => {
                tracing::warn!(
                    manifest = %file.display(),
                    %error,
                    "external tool manifest skipped"
                );
            }
        }
    }
}

/// A manifest-backed tool. Implements the same contract as built-ins; the
/// manifest supplies the schema, description, and executable.
pub struct ExternalTool {
    manifest: Manifest,
    schema: Value,
    manifest_dir: PathBuf,
    name: &'static str,
    description: &'static str,
}

impl ExternalTool {
    pub fn new(loaded: LoadedManifest) -> Self {
        // `Tool::name`/`Tool::description` return `&'static str`, while manifests
        // load at runtime. Intern each distinct string exactly once so repeated
        // `/profile` reloads reuse the same allocation; memory stays bounded by
        // the number of distinct manifests, not by reload count.
        let name = intern(&loaded.manifest.name);
        let description = intern(&truncate_description(&loaded.manifest.description));
        Self {
            manifest: loaded.manifest,
            schema: loaded.schema,
            manifest_dir: loaded.manifest_dir,
            name,
            description,
        }
    }

    pub fn manifest_dir(&self) -> &Path {
        &self.manifest_dir
    }

    fn prepare(
        &self,
        ctx: &ToolContext,
        args: &Map<String, Value>,
    ) -> Result<PreparedCall, String> {
        let substituted = substitute_args(&self.manifest, args)?;
        let (program, argv) = if self.manifest.shell {
            // Keep argument boundaries: `<command> "$@"` runs the manifest
            // command with the substituted arguments, under `sh -c`.
            let mut argv = Vec::with_capacity(substituted.len() + 3);
            argv.push("-c".to_string());
            argv.push(format!("{} \"$@\"", self.manifest.command));
            argv.push("localharness-external".to_string());
            argv.extend(substituted);
            ("/bin/sh".to_string(), argv)
        } else {
            (self.manifest.command.clone(), substituted)
        };

        if program.contains('\0') || argv.iter().any(|arg| arg.contains('\0')) {
            return Err(format!(
                "manifest `{}` produced an argument containing a NUL byte; remove it from the arguments",
                self.name
            ));
        }

        let request = json!({
            "tool": self.name,
            "args": Value::Object(args.clone()),
            "cwd": ctx.cwd.display().to_string(),
            "session_id": session_id(ctx),
        });
        let mut stdin = serde_json::to_vec(&request)
            .map_err(|error| format!("cannot encode the request for `{}`: {error}", self.name))?;
        stdin.push(b'\n');

        Ok(PreparedCall {
            program,
            args: argv,
            env: self.manifest.env.clone(),
            cwd: ctx.cwd.clone(),
            stdin,
            timeout: self.manifest.timeout(),
        })
    }

    /// Best-effort scope gate. The probe contains the manifest command plus all
    /// string argument values so `gate` can recognise a known offensive tool and
    /// extract its targets.
    fn check_scope(&self, ctx: &ToolContext, args: &Map<String, Value>) -> Result<(), String> {
        let mut probe = self.manifest.command.clone();
        let mut values = Vec::new();
        collect_strings(&Value::Object(args.clone()), &mut values);
        for value in values {
            probe.push(' ');
            probe.push_str(&value);
        }
        let security = match ctx.security.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        security
            .gate(&probe)
            .map_err(|message| format!("{message}; external tool `{}` was not started", self.name))
    }

    fn interpret(&self, output: RunOutput) -> ToolResult {
        let exit_ok = output.code == Some(0);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let text = stdout.trim();

        if text.is_empty() {
            if exit_ok {
                return ToolResult {
                    content: cap_content(format!("`{}` produced no output", self.name)),
                    is_error: false,
                    details: Some(json!({"exit_code": output.code})),
                };
            }
            return self.exit_error(&output);
        }

        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text)
            && (map.contains_key("content")
                || map.contains_key("is_error")
                || map.contains_key("details"))
        {
            return self.protocol_result(map, &output);
        }

        if !exit_ok {
            return self.exit_error(&output);
        }

        ToolResult {
            content: cap_content(
                text.trim_end_matches(|c: char| c.is_whitespace())
                    .to_string(),
            ),
            is_error: false,
            details: Some(json!({
                "exit_code": output.code,
                "stdout_truncated": output.stdout_truncated,
            })),
        }
    }

    /// The tool spoke the protocol: `{content, is_error, details}`.
    fn protocol_result(&self, mut map: Map<String, Value>, output: &RunOutput) -> ToolResult {
        let stderr = stderr_snippet(output);
        let content = match map.remove("content") {
            Some(Value::String(content)) => content,
            Some(_) => {
                return ToolResult::err(
                    "EXTERNAL_JSON",
                    format!(
                        "`{}` returned a non-string `content`; print {{\"content\": \"...\", \"is_error\": false, \"details\": {{...}}}}{stderr}",
                        self.name
                    ),
                );
            }
            None => {
                return ToolResult::err(
                    "EXTERNAL_JSON",
                    format!(
                        "`{}` returned JSON without `content`; print {{\"content\": \"...\", \"is_error\": false}}{stderr}",
                        self.name
                    ),
                );
            }
        };
        let is_error = match map.remove("is_error") {
            Some(Value::Bool(value)) => value,
            Some(Value::Null) | None => output.code != Some(0),
            Some(_) => {
                return ToolResult::err(
                    "EXTERNAL_JSON",
                    format!(
                        "`{}` returned a non-boolean `is_error`; use true or false{stderr}",
                        self.name
                    ),
                );
            }
        };
        let details = match map.remove("details") {
            None | Some(Value::Null) => None,
            Some(value) => Some(cap_details(value)),
        };
        let content = if is_error && !stderr.is_empty() {
            let stderr = stderr.trim_start_matches("; ");
            format!("{content}\n\nstderr: {stderr}")
        } else {
            content
        };
        ToolResult {
            content: cap_content(content),
            is_error,
            details,
        }
    }

    fn exit_error(&self, output: &RunOutput) -> ToolResult {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let status = match output.code {
            Some(code) => format!("exited with status {code}"),
            None => "was terminated by a signal".to_string(),
        };
        let mut message = format!("external tool `{}` {status}", self.name);
        if !stderr.trim().is_empty() {
            message.push_str(&format!(
                "; stderr: {}",
                truncate_text(stderr.trim(), STDERR_MESSAGE_CAP)
            ));
        }
        if !stdout.trim().is_empty() {
            message.push_str(&format!(
                "\n\nstdout:\n{}",
                truncate_text(stdout.trim(), STDERR_MESSAGE_CAP)
            ));
        }
        message.push_str("\n\nFix the command or arguments and retry.");
        ToolResult {
            content: cap_content(format!("[EXTERNAL_EXIT] {message}")),
            is_error: true,
            details: Some(json!({
                "code": "EXTERNAL_EXIT",
                "exit_code": output.code,
            })),
        }
    }

    fn run_error(&self, error: RunError) -> ToolResult {
        match &error {
            RunError::Cancelled => ToolResult::err(
                "cancelled",
                format!(
                    "external tool `{}` was cancelled; retry the call when ready",
                    self.name
                ),
            ),
            RunError::Timeout {
                elapsed, stderr, ..
            } => {
                let mut message = format!(
                    "external tool `{}` timed out after {elapsed:?} and its process group was killed; raise `timeout_secs` in the manifest or run long scans with `bg`",
                    self.name
                );
                if !stderr.trim().is_empty() {
                    message.push_str(&format!(
                        "; stderr: {}",
                        truncate_text(stderr.trim(), STDERR_MESSAGE_CAP)
                    ));
                }
                ToolResult::err("EXTERNAL_TIMEOUT", message)
            }
            RunError::Spawn { .. } | RunError::Io { .. } => {
                ToolResult::err("EXTERNAL_SPAWN", error.to_string())
            }
        }
    }
}

#[async_trait]
impl Tool for ExternalTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    fn pack(&self) -> ToolPack {
        // External tools never auto-join a pack: `ToolRegistry::select` adds
        // them explicitly after pack selection.
        ToolPack::Core
    }

    fn read_only(&self) -> bool {
        false
    }

    fn evidence_enabled(&self) -> bool {
        true
    }

    fn evidence_command(&self, args: &Value) -> Option<String> {
        let args = serde_json::to_string(args).unwrap_or_else(|_| "{}".into());
        Some(format!("{} {}", self.manifest.command, args))
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        if ctx.is_cancelled() {
            return ToolResult::err(
                "cancelled",
                format!(
                    "external tool `{}` was not started because the turn was cancelled; retry when ready",
                    self.name
                ),
            );
        }

        let args = match args {
            Value::Object(object) => object,
            Value::Null => Map::new(),
            _ => {
                return ToolResult::err(
                    "EXTERNAL_ARGS",
                    format!(
                        "arguments for `{}` must be a JSON object matching its schema",
                        self.name
                    ),
                );
            }
        };

        if let Err(message) = validate_args(self.name, &args, &self.schema) {
            return ToolResult::err("EXTERNAL_ARGS", message);
        }

        if self.manifest.scope_gate
            && let Err(message) = self.check_scope(ctx, &args)
        {
            return ToolResult::err("SEC_SCOPE", message);
        }

        let prepared = match self.prepare(ctx, &args) {
            Ok(prepared) => prepared,
            Err(message) => return ToolResult::err("EXTERNAL_ARGS", message),
        };

        match runner::run(prepared, ctx.cancel.clone()).await {
            Ok(output) => self.interpret(output),
            Err(error) => self.run_error(error),
        }
    }
}

/// Interner for the `'static` tool metadata required by the `Tool` trait.
///
/// The shared trait cannot return borrowed strings, so dynamically loaded tool
/// names and descriptions must be promoted to `'static`. Deduplicating them
/// keeps the promotion bounded: a `/profile` reload finds the existing entry
/// instead of allocating again.
struct Interner {
    strings: Mutex<HashMap<String, &'static str>>,
}

static INTERNER: LazyLock<Interner> = LazyLock::new(|| Interner {
    strings: Mutex::new(HashMap::new()),
});

fn intern(text: &str) -> &'static str {
    let mut strings = match INTERNER.strings.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(existing) = strings.get(text) {
        return existing;
    }
    let promoted: &'static str = String::from(text).leak();
    strings.insert(text.to_string(), promoted);
    promoted
}

fn placeholder_regex() -> &'static Regex {
    static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\{([A-Za-z_][A-Za-z0-9_]*)\}").expect("placeholder pattern is valid")
    });
    &PLACEHOLDER
}

/// Substitute `{key}` placeholders in manifest arguments with validated scalar
/// argument values. Arrays and objects are rejected: the tool can read them
/// from stdin instead.
fn substitute_args(manifest: &Manifest, args: &Map<String, Value>) -> Result<Vec<String>, String> {
    manifest
        .args
        .iter()
        .map(|template| substitute_one(manifest, template, args))
        .collect()
}

fn substitute_one(
    manifest: &Manifest,
    template: &str,
    args: &Map<String, Value>,
) -> Result<String, String> {
    let regex = placeholder_regex();
    let mut out = String::with_capacity(template.len());
    let mut last = 0;
    for captures in regex.captures_iter(template) {
        let whole = captures.get(0).expect("capture 0 always matches");
        let key = captures.get(1).expect("capture 1 always matches").as_str();
        out.push_str(&template[last..whole.start()]);
        let value = args.get(key).ok_or_else(|| {
            format!(
                "argument `{key}` is required by manifest `{}` (placeholder {{{key}}}) but was not provided; call `{}` with {{\"{key}\": \"...\"}}",
                manifest.name, manifest.name
            )
        })?;
        out.push_str(&scalar_text(manifest, key, value)?);
        last = whole.end();
    }
    out.push_str(&template[last..]);
    Ok(out)
}

fn scalar_text(manifest: &Manifest, key: &str, value: &Value) -> Result<String, String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Number(number) => Ok(number.to_string()),
        Value::Bool(flag) => Ok(flag.to_string()),
        Value::Null => Err(format!(
            "argument `{key}` is null but manifest `{}` substitutes it into an argument; pass a string, number, or boolean",
            manifest.name
        )),
        Value::Array(_) => Err(format!(
            "argument `{key}` is an array but manifest `{}` substitutes it into one argument; pass a scalar or let the tool read the array from stdin",
            manifest.name
        )),
        Value::Object(_) => Err(format!(
            "argument `{key}` is an object but manifest `{}` substitutes it into one argument; pass a scalar or let the tool read the object from stdin",
            manifest.name
        )),
    }
}

/// Validate required keys and declared primitive types. Unknown keys are
/// allowed: they still reach the tool on stdin.
fn validate_args(tool: &str, args: &Map<String, Value>, schema: &Value) -> Result<(), String> {
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for key in required.iter().filter_map(Value::as_str) {
            if !args.contains_key(key) {
                return Err(format!(
                    "missing required argument `{key}`; call `{tool}` with {{\"{key}\": \"...\"}}"
                ));
            }
        }
    }

    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (key, spec) in properties {
            let Some(value) = args.get(key) else {
                continue;
            };
            let Some(expected) = spec.get("type") else {
                continue;
            };
            if !matches_type(value, expected) {
                return Err(format!(
                    "argument `{key}` must be {}, got {}; check the call against the `{tool}` schema",
                    type_label(expected),
                    actual_type(value)
                ));
            }
        }
    }

    Ok(())
}

fn matches_type(value: &Value, expected: &Value) -> bool {
    match expected {
        Value::String(kind) => matches_single_type(value, kind),
        Value::Array(kinds) => kinds
            .iter()
            .filter_map(Value::as_str)
            .any(|kind| matches_single_type(value, kind)),
        _ => true,
    }
}

fn matches_single_type(value: &Value, kind: &str) -> bool {
    match kind {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        // Unknown schema keywords are not enforced.
        _ => true,
    }
}

fn type_label(expected: &Value) -> String {
    match expected {
        Value::String(kind) => kind.clone(),
        Value::Array(kinds) => {
            let names: Vec<&str> = kinds.iter().filter_map(Value::as_str).collect();
            if names.is_empty() {
                "a supported JSON value".to_string()
            } else {
                names.join(" or ")
            }
        }
        _ => "a supported JSON value".to_string(),
    }
}

fn actual_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn collect_strings(value: &Value, into: &mut Vec<String>) {
    match value {
        Value::String(text) => into.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                collect_strings(item, into);
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                collect_strings(item, into);
            }
        }
        _ => {}
    }
}

fn session_id(ctx: &ToolContext) -> String {
    match ctx.session.lock() {
        Ok(session) => session.id.clone(),
        Err(poisoned) => poisoned.into_inner().id.clone(),
    }
}

fn truncate_description(text: &str) -> String {
    const LIMIT: usize = 197;
    let text = text.trim();
    if text.chars().count() <= LIMIT {
        return text.to_string();
    }
    let mut out: String = text.chars().take(LIMIT.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

fn cap_content(content: String) -> String {
    if content.len() <= CONTENT_CAP {
        return content;
    }
    let mut end = CONTENT_CAP;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n(truncated; external tool output capped at {} KiB)",
        &content[..end],
        CONTENT_CAP / 1024
    )
}

fn cap_details(value: Value) -> Value {
    match serde_json::to_string(&value) {
        Ok(serialized) if serialized.len() <= DETAILS_CAP => value,
        Ok(serialized) => json!({
            "omitted": format!(
                "details were {} bytes, over the {} byte cap, and were dropped",
                serialized.len(),
                DETAILS_CAP
            )
        }),
        Err(_) => json!({"omitted": "details could not be serialized"}),
    }
}

fn truncate_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

/// `; stderr: ...` suffix for error messages; empty when stderr is empty.
fn stderr_snippet(output: &RunOutput) -> String {
    let text = String::from_utf8_lossy(&output.stderr);
    let text = text.trim();
    if text.is_empty() {
        String::new()
    } else {
        format!("; stderr: {}", truncate_text(text, STDERR_MESSAGE_CAP))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VerifyConfig;
    use crate::security::SecurityState;
    use crate::session::{Session, SessionData};
    use crate::tools::{BgRegistry, ReadTracker};
    use std::sync::Mutex;

    const TARGET_SCHEMA: &str =
        r#"{"type":"object","properties":{"target":{"type":"string"}},"required":["target"]}"#;
    const EMPTY_SCHEMA: &str = r#"{"type":"object","properties":{}}"#;

    fn base_manifest(name: &str) -> Manifest {
        Manifest {
            name: name.to_string(),
            description: "test external tool".to_string(),
            command: "/bin/true".to_string(),
            args: Vec::new(),
            input_schema: "schema.json".to_string(),
            timeout_secs: 10,
            shell: false,
            scope_gate: false,
            env: BTreeMap::new(),
        }
    }

    fn loaded(dir: &Path, manifest: Manifest, schema: &str) -> LoadedManifest {
        LoadedManifest {
            manifest,
            schema: serde_json::from_str(schema).unwrap(),
            manifest_dir: dir.to_path_buf(),
        }
    }

    fn context(dir: &Path, security: SecurityState) -> ToolContext {
        let paths = Paths {
            config_dir: dir.join("config"),
            data_dir: dir.join("data"),
            cwd: dir.to_path_buf(),
            project_id: "test".to_string(),
            sessions_dir_override: None,
        };
        let header = SessionData {
            cwd: dir.display().to_string(),
            endpoint: "http://127.0.0.1:9".to_string(),
            model: "test".to_string(),
            profile: "full".to_string(),
            app: "test".to_string(),
            context: None,
        };
        let session = Session::create(&paths, header, true).unwrap();
        let artifacts = session.artifacts.clone();
        ToolContext {
            cwd: dir.to_path_buf(),
            path_jail: false,
            session: Arc::new(Mutex::new(session)),
            artifacts,
            cancel: tokio_util::sync::CancellationToken::new(),
            trust: true,
            verify: VerifyConfig::default(),
            profile: "full".to_string(),
            reads: Arc::new(Mutex::new(ReadTracker::default())),
            bg: Arc::new(Mutex::new(BgRegistry::new())),
            security: Arc::new(Mutex::new(security)),
            paths,
            lsp: Arc::new(crate::tools::lsp::LspService::new()),
            sandbox: None,
        }
    }

    #[test]
    fn substitute_placeholders_from_scalars() {
        let mut manifest = base_manifest("demo");
        manifest.args = vec![
            "--target".to_string(),
            "{target}".to_string(),
            "--count={count}".to_string(),
            "--flag={flag}".to_string(),
        ];
        let args = json!({"target": "example.com", "count": 3, "flag": true});
        let substituted = substitute_args(&manifest, args.as_object().unwrap()).unwrap();
        assert_eq!(
            substituted,
            vec!["--target", "example.com", "--count=3", "--flag=true"]
        );
    }

    #[test]
    fn substitute_rejects_arrays_and_objects() {
        let mut manifest = base_manifest("demo");
        manifest.args = vec!["{targets}".to_string()];
        let args = json!({"targets": ["a", "b"]});
        let error = substitute_args(&manifest, args.as_object().unwrap()).unwrap_err();
        assert!(error.contains("array"), "{error}");

        manifest.args = vec!["{opts}".to_string()];
        let args = json!({"opts": {"a": 1}});
        let error = substitute_args(&manifest, args.as_object().unwrap()).unwrap_err();
        assert!(error.contains("object"), "{error}");
    }

    #[test]
    fn substitute_names_a_missing_argument() {
        let mut manifest = base_manifest("demo");
        manifest.args = vec!["{target}".to_string()];
        let args = json!({});
        let error = substitute_args(&manifest, args.as_object().unwrap()).unwrap_err();
        assert!(error.contains("target"), "{error}");
        assert!(error.contains("demo"), "{error}");
    }

    #[test]
    fn validates_required_and_types() {
        let schema: Value = serde_json::from_str(TARGET_SCHEMA).unwrap();
        let mut args = Map::new();
        let error = validate_args("demo", &args, &schema).unwrap_err();
        assert!(
            error.contains("missing required argument `target`"),
            "{error}"
        );

        args.insert("target".to_string(), json!(42));
        let error = validate_args("demo", &args, &schema).unwrap_err();
        assert!(error.contains("must be string"), "{error}");

        args.insert("target".to_string(), json!("example.com"));
        validate_args("demo", &args, &schema).unwrap();
    }

    #[cfg(unix)]
    fn write_script(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("tool.sh");
        std::fs::write(&path, body).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn non_json_stdout_is_plain_text() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "#!/bin/sh\nprintf 'plain findings\\n'\n");
        let mut manifest = base_manifest("emit");
        manifest.command = script.display().to_string();
        let tool = ExternalTool::new(loaded(dir.path(), manifest, EMPTY_SCHEMA));
        let ctx = context(dir.path(), SecurityState::default());

        let result = tool.call(&ctx, json!({})).await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content.trim(), "plain findings");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn json_protocol_result_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            dir.path(),
            "#!/bin/sh\nprintf '{\"content\":\"ok\",\"is_error\":false,\"details\":{\"n\":1}}'\n",
        );
        let mut manifest = base_manifest("emit");
        manifest.command = script.display().to_string();
        let tool = ExternalTool::new(loaded(dir.path(), manifest, EMPTY_SCHEMA));
        let ctx = context(dir.path(), SecurityState::default());

        let result = tool.call(&ctx, json!({})).await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "ok");
        assert_eq!(result.details.unwrap()["n"], json!(1));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_exit_is_an_external_exit_error() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "#!/bin/sh\necho boom >&2\nexit 3\n");
        let mut manifest = base_manifest("fail");
        manifest.command = script.display().to_string();
        let tool = ExternalTool::new(loaded(dir.path(), manifest, EMPTY_SCHEMA));
        let ctx = context(dir.path(), SecurityState::default());

        let result = tool.call(&ctx, json!({})).await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with("[EXTERNAL_EXIT]"),
            "{}",
            result.content
        );
        assert!(result.content.contains("boom"), "{}", result.content);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_is_reported_with_the_runner_code() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = base_manifest("slow");
        manifest.command = "/bin/sh".to_string();
        manifest.args = vec!["-c".to_string(), "sleep 5".to_string()];
        manifest.timeout_secs = 1;
        let tool = ExternalTool::new(loaded(dir.path(), manifest, EMPTY_SCHEMA));
        let ctx = context(dir.path(), SecurityState::default());

        let result = tool.call(&ctx, json!({})).await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with("[EXTERNAL_TIMEOUT]"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn scope_gate_blocks_out_of_scope_targets_before_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let mut manifest = base_manifest("scan");
        manifest.command = "nmap".to_string();
        manifest.args = vec!["-p".to_string(), "80".to_string(), "{target}".to_string()];
        manifest.scope_gate = true;
        let tool = ExternalTool::new(loaded(dir.path(), manifest, TARGET_SCHEMA));
        // An empty scope denies every target.
        let ctx = context(dir.path(), SecurityState::default());

        let result = tool.call(&ctx, json!({"target": "example.com"})).await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with("[SEC_SCOPE]"),
            "{}",
            result.content
        );
        assert!(result.content.contains("example.com"), "{}", result.content);
        assert!(result.content.contains("not started"), "{}", result.content);
    }

    #[test]
    fn description_stays_under_two_hundred_characters() {
        let long = "x".repeat(500);
        assert!(truncate_description(&long).chars().count() < 200);
        assert_eq!(truncate_description("short"), "short");
    }

    #[test]
    fn content_cap_truncates_on_a_char_boundary() {
        let text = "é".repeat(CONTENT_CAP);
        let capped = cap_content(text);
        assert!(capped.starts_with('é'));
        assert!(capped.contains("truncated"));
    }

    #[test]
    fn details_cap_drops_oversized_payloads() {
        let value = json!({"blob": "x".repeat(DETAILS_CAP + 1)});
        let capped = cap_details(value);
        assert!(capped.get("omitted").is_some());
    }
}
