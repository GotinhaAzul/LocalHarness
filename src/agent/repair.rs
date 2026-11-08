//! Tool-argument repair and validation. Runs before JSON Schema validation and
//! is deliberately boring and observable.

use serde_json::{Map, Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairError {
    pub code: &'static str,
    pub message: String,
}

impl RepairError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Canonical JSON with recursively sorted object keys, used for loop-guard
/// signatures.
pub fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = String::from("{");
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).unwrap_or_default());
                out.push(':');
                out.push_str(&canonical_json(&map[*key]));
            }
            out.push('}');
            out
        }
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", parts.join(","))
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Parse a raw argument payload. Accepts a JSON object, or a JSON string that
/// contains one (some providers double-encode).
pub fn parse_raw(raw: &str) -> Result<Value, RepairError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(json!({}));
    }
    let mut value: Value = serde_json::from_str(raw).map_err(|err| {
        RepairError::new(
            "bad_arguments",
            format!(
                "arguments are not valid JSON ({err}); pass one JSON object, for example {{\"path\":\"src/main.rs\"}}"
            ),
        )
    })?;
    if let Value::String(inner) = &value
        && inner.trim_start().starts_with('{')
    {
        value = serde_json::from_str(inner).map_err(|err| {
            RepairError::new(
                "bad_arguments",
                format!("double-encoded arguments could not be parsed: {err}"),
            )
        })?;
    }
    match value {
        Value::Object(_) => Ok(value),
        Value::Null => Ok(json!({})),
        _ => Err(RepairError::new(
            "bad_arguments",
            "arguments must be a JSON object, not a scalar or array",
        )),
    }
}

impl From<RepairError> for crate::tools::ToolResult {
    fn from(err: RepairError) -> Self {
        crate::tools::ToolResult::err(err.code, err.message)
    }
}

/// Argument aliases as `(tool, alias, canonical)`; `*` applies to every tool.
/// Local models are trained on many harnesses and reach for their argument
/// names by reflex (`file_path`, `old_string`, ...). An alias only applies when
/// the tool declares the canonical key and does not declare the alias itself,
/// and never overwrites a canonical key the model also sent.
const ALIASES: &[(&str, &str, &str)] = &[
    ("*", "file", "path"),
    ("*", "file_path", "path"),
    ("*", "filepath", "path"),
    ("*", "filename", "path"),
    ("*", "absolute_path", "path"),
    ("*", "target_file", "path"),
    ("*", "cmd", "command"),
    ("*", "startLine", "start"),
    ("*", "start_line", "start"),
    ("*", "endLine", "end"),
    ("*", "end_line", "end"),
    ("*", "timeout_secs", "timeout"),
    ("*", "timeout_seconds", "timeout"),
    ("read", "offset", "start"),
    ("read", "line", "start"),
    ("write", "contents", "content"),
    ("write", "text", "content"),
    ("write", "file_text", "content"),
    ("edit", "old_string", "old"),
    ("edit", "old_str", "old"),
    ("edit", "oldText", "old"),
    ("edit", "old_text", "old"),
    ("edit", "search", "old"),
    ("edit", "new_string", "new"),
    ("edit", "new_str", "new"),
    ("edit", "newText", "new"),
    ("edit", "new_text", "new"),
    ("edit", "replace", "new"),
    ("edit", "replacement", "new"),
    ("edit", "replaceAll", "replace_all"),
    ("edit", "all", "replace_all"),
    ("patch", "old", "anchor"),
    ("patch", "old_string", "anchor"),
    ("patch", "old_str", "anchor"),
    ("patch", "oldText", "anchor"),
    ("patch", "old_text", "anchor"),
    ("patch", "search", "anchor"),
    ("patch", "new", "replacement"),
    ("patch", "new_string", "replacement"),
    ("patch", "new_str", "replacement"),
    ("patch", "newText", "replacement"),
    ("patch", "new_text", "replacement"),
    ("patch", "replace", "replacement"),
    ("bash", "script", "command"),
    ("bash", "workdir", "cwd"),
    ("bash", "working_directory", "cwd"),
    ("bash", "directory", "cwd"),
    ("grep", "query", "pattern"),
    ("grep", "regex", "pattern"),
    ("grep", "search", "pattern"),
    ("grep", "include", "glob"),
    ("grep", "file_pattern", "glob"),
    ("grep", "directory", "path"),
    ("grep", "dir", "path"),
    ("grep", "context_lines", "context"),
    ("grep", "maxResults", "max_matches"),
    ("grep", "max_results", "max_matches"),
    ("grep", "limit", "max_matches"),
    ("glob", "glob_pattern", "pattern"),
    ("glob", "glob", "pattern"),
    ("glob", "directory", "path"),
    ("glob", "dir", "path"),
    ("recall", "id", "artifact"),
    ("recall", "artifact_id", "artifact"),
    ("todo", "items", "tasks"),
    ("todo", "todos", "tasks"),
    ("todo", "steps", "tasks"),
    ("test", "name", "test"),
    ("test", "filter", "test"),
    ("bg", "process", "id"),
    ("bg", "pid", "id"),
];

/// Enum values models commonly reach for, as `(tool, key, synonym, canonical)`.
const ENUM_SYNONYMS: &[(&str, &str, &str, &str)] = &[
    ("todo", "action", "complete", "done"),
    ("todo", "action", "completed", "done"),
    ("todo", "action", "finish", "done"),
    ("todo", "action", "finished", "done"),
    ("todo", "action", "status", "list"),
    ("todo", "action", "show", "list"),
    ("todo", "action", "create", "plan"),
    ("todo", "action", "begin", "start"),
    ("bg", "action", "run", "start"),
    ("bg", "action", "spawn", "start"),
    ("bg", "action", "kill", "stop"),
    ("bg", "action", "terminate", "stop"),
    ("bg", "action", "log", "logs"),
    ("bg", "action", "output", "logs"),
    ("bg", "action", "tail", "logs"),
    ("bg", "action", "list", "status"),
    ("bg", "action", "ps", "status"),
    ("lsp", "action", "symbols", "outline"),
    ("lsp", "action", "document-symbols", "outline"),
    ("lsp", "action", "def", "definition"),
    ("lsp", "action", "goto-definition", "definition"),
    ("lsp", "action", "refs", "references"),
    ("lsp", "action", "find-references", "references"),
    ("lsp", "action", "diagnostic", "diagnostics"),
    ("lsp", "action", "errors", "diagnostics"),
    ("finding", "action", "create", "add"),
    ("finding", "action", "new", "add"),
    ("finding", "action", "edit", "update"),
    ("finding", "action", "modify", "update"),
    ("thread", "action", "info", "status"),
    ("thread", "action", "fork", "split"),
];

fn apply_aliases(tool: &str, map: &mut Map<String, Value>, schema: &Value) {
    let properties = schema.get("properties").and_then(|p| p.as_object());
    let declared = |key: &str| properties.is_some_and(|props| props.contains_key(key));
    for (scope, alias, canonical) in ALIASES {
        if (*scope != "*" && *scope != tool) || declared(alias) || !declared(canonical) {
            continue;
        }
        if let Some(value) = map.remove(*alias)
            && !map.contains_key(*canonical)
        {
            map.insert((*canonical).to_string(), value);
        }
    }
}

/// Fill in `action` when the model omitted it but the other arguments make
/// the intent unambiguous.
fn infer_action(tool: &str, map: &mut Map<String, Value>) {
    if map.contains_key("action") {
        return;
    }
    let action = match tool {
        "todo" if map.contains_key("tasks") => "plan",
        "bg" if map.contains_key("command") => "start",
        "finding" if map.contains_key("title") && map.contains_key("severity") => "add",
        _ => return,
    };
    map.insert("action".to_string(), json!(action));
}

/// Fold an enum value to its canonical spelling: case, surrounding space, and
/// `_`/space vs `-` are ignored, and a few common synonyms are accepted.
fn fold_enum(tool: &str, key: &str, value: Value, property: &Value) -> Value {
    let Some(allowed) = property.get("enum").and_then(|e| e.as_array()) else {
        return value;
    };
    let Value::String(text) = &value else {
        return value;
    };
    if allowed.contains(&value) {
        return value;
    }
    let normalize = |s: &str| s.trim().to_ascii_lowercase().replace(['_', ' '], "-");
    let wanted = normalize(text);
    let mut hits = allowed
        .iter()
        .filter_map(Value::as_str)
        .filter(|candidate| normalize(candidate) == wanted);
    if let (Some(hit), None) = (hits.next(), hits.next()) {
        return json!(hit);
    }
    for (scope, field, synonym, canonical) in ENUM_SYNONYMS {
        if *scope == tool && *field == key && *synonym == wanted {
            return json!(canonical);
        }
    }
    value
}

fn apply_defaults(tool: &str, map: &mut Map<String, Value>) {
    match tool {
        "bash" => {
            map.entry("timeout".to_string()).or_insert(json!(120));
        }
        "grep" => {
            map.entry("max_matches".to_string()).or_insert(json!(200));
            map.entry("context".to_string()).or_insert(json!(0));
        }
        "recall" => {
            map.entry("start".to_string()).or_insert(json!(1));
            map.entry("end".to_string()).or_insert(json!(120));
        }
        "review" => {
            map.entry("lines".to_string()).or_insert(json!(400));
        }
        _ => {}
    }
}

fn coerce(value: Value, schema: &Value) -> Value {
    let expected = schema.get("type").and_then(|t| t.as_str());
    if let Some(expected) = expected {
        if expected != "array"
            && matches!(&value, Value::Array(items) if items.len() == 1)
            && let Value::Array(mut items) = value
        {
            return coerce(items.remove(0), schema);
        }
        if expected == "integer"
            && let Some(float) = value.as_f64()
            && !value.is_i64()
            && !value.is_u64()
            && float.fract() == 0.0
        {
            return json!(float as i64);
        }
        if expected == "string" && (value.is_number() || value.is_boolean()) {
            return json!(value.to_string());
        }
        if let Value::String(text) = &value {
            match expected {
                "integer" => {
                    if let Ok(parsed) = text.trim().parse::<i64>() {
                        return json!(parsed);
                    }
                    if let Ok(parsed) = text.trim().parse::<f64>()
                        && parsed.fract() == 0.0
                    {
                        return json!(parsed as i64);
                    }
                }
                "number" => {
                    if let Ok(parsed) = text.trim().parse::<f64>() {
                        return json!(parsed);
                    }
                }
                "boolean" => match text.trim().to_ascii_lowercase().as_str() {
                    "true" | "yes" | "1" => return json!(true),
                    "false" | "no" | "0" => return json!(false),
                    _ => {}
                },
                "array" => return coerce_array(text, schema),
                _ => {}
            }
        }
    }
    value
}

/// Models often send an array as a JSON-encoded string or as a bulleted list.
fn coerce_array(text: &str, schema: &Value) -> Value {
    let trimmed = text.trim();
    if trimmed.starts_with('[')
        && let Ok(parsed @ Value::Array(_)) = serde_json::from_str::<Value>(trimmed)
    {
        return parsed;
    }
    let wants_strings = schema
        .get("items")
        .and_then(|items| items.get("type"))
        .and_then(Value::as_str)
        == Some("string");
    if !wants_strings {
        return Value::String(text.to_string());
    }
    let items: Vec<Value> = trimmed
        .lines()
        .map(strip_list_marker)
        .filter(|line| !line.is_empty())
        .map(|line| json!(line))
        .collect();
    Value::Array(items)
}

/// Strip a leading `- `, `* `, `1. `, or `1) ` list marker.
fn strip_list_marker(line: &str) -> &str {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
        return rest.trim();
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0
        && let Some(rest) = line[digits..]
            .strip_prefix(". ")
            .or_else(|| line[digits..].strip_prefix(") "))
    {
        return rest.trim();
    }
    line
}

/// Remove keys the schema does not declare (`additionalProperties: false`).
fn drop_unknown(map: &mut Map<String, Value>, schema: &Value) -> Vec<String> {
    let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) else {
        return Vec::new();
    };
    let mut dropped = Vec::new();
    map.retain(|key, _| {
        let known = properties.contains_key(key);
        if !known {
            dropped.push(key.clone());
        }
        known
    });
    dropped
}

/// Repaired arguments plus notes the model should see alongside the result.
#[derive(Debug, Clone, PartialEq)]
pub struct Repaired {
    pub args: Value,
    /// Arguments that were dropped because the tool does not declare them.
    pub ignored: Vec<String>,
}

impl Repaired {
    /// A one-line note for the tool result, or `None` when nothing was dropped.
    pub fn note(&self, tool: &str, schema: &Value) -> Option<String> {
        if self.ignored.is_empty() {
            return None;
        }
        Some(format!(
            "note: ignored unknown argument(s) {}; {tool} accepts {}",
            self.ignored.join(", "),
            accepted_keys(schema)
        ))
    }
}

fn accepted_keys(schema: &Value) -> String {
    schema
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|props| props.keys().cloned().collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

/// Repair and validate tool arguments.
pub fn repair(tool: &str, raw: &str, schema: &Value) -> Result<Value, RepairError> {
    repair_with_notes(tool, raw, schema).map(|repaired| repaired.args)
}

/// Repair and validate tool arguments, reporting what was dropped.
pub fn repair_with_notes(tool: &str, raw: &str, schema: &Value) -> Result<Repaired, RepairError> {
    let mut value = parse_raw(raw)?;
    let Value::Object(map) = &mut value else {
        return Err(RepairError::new(
            "bad_arguments",
            "arguments must be an object",
        ));
    };
    // `null` means "not provided" for every model we have seen.
    map.retain(|_, value| !value.is_null());
    apply_aliases(tool, map, schema);
    infer_action(tool, map);
    apply_defaults(tool, map);
    if let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) {
        for (key, property) in properties {
            if let Some(existing) = map.remove(key) {
                let coerced = coerce(existing, property);
                map.insert(key.clone(), fold_enum(tool, key, coerced, property));
            }
        }
    }
    let ignored = drop_unknown(map, schema);

    if let Err(mut err) = validate(tool, &value, schema) {
        if !ignored.is_empty() {
            err.message.push_str(&format!(
                " (ignored unknown argument(s) {}; {tool} accepts {})",
                ignored.join(", "),
                accepted_keys(schema)
            ));
        }
        return Err(err);
    }
    reject_noops(tool, &value)?;
    if tool == "bash"
        && let Some(command) = value.get("command").and_then(|v| v.as_str())
        && let Some(blocked) = interactive_command(command)
    {
        return Err(RepairError::new(
            "interactive_command",
            format!(
                "`{blocked}` is interactive and would hang the harness. {}. Non-interactive alternative: bash with a single command, or bg for long-running work.",
                remedy_for(blocked)
            ),
        ));
    }
    Ok(Repaired {
        args: value,
        ignored,
    })
}

fn validate(tool: &str, value: &Value, schema: &Value) -> Result<(), RepairError> {
    let Some(map) = value.as_object() else {
        return Err(RepairError::new(
            "bad_arguments",
            "arguments must be an object",
        ));
    };
    let required = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|items| items.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();
    for key in &required {
        if !map.contains_key(*key) {
            let example = example_for(tool);
            return Err(RepairError::new(
                "missing_argument",
                format!("{tool} requires \"{key}\"; call {tool} with {example}"),
            ));
        }
    }
    if let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) {
        for (key, property) in properties {
            let Some(actual) = map.get(key) else {
                continue;
            };
            let expected = property.get("type").and_then(|t| t.as_str());
            let matches = match expected {
                Some("string") => actual.is_string(),
                Some("integer") => actual.is_i64() || actual.is_u64(),
                Some("number") => actual.is_number(),
                Some("boolean") => actual.is_boolean(),
                Some("array") => actual.is_array(),
                Some("object") => actual.is_object(),
                _ => true,
            };
            if !matches {
                return Err(RepairError::new(
                    "bad_arguments",
                    format!(
                        "\"{key}\" must be {}, got {}; fix the value and retry {tool}",
                        expected.unwrap_or("a value"),
                        json_type(actual)
                    ),
                ));
            }
            if let Some(allowed) = property.get("enum").and_then(|e| e.as_array())
                && !allowed.contains(actual)
            {
                let options = allowed
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(RepairError::new(
                    "bad_arguments",
                    format!("\"{key}\" must be one of {options}; got {actual}"),
                ));
            }
            if let Some(items_schema) = property.get("items")
                && let Some(items) = actual.as_array()
            {
                for item in items {
                    if !matches_type(item, items_schema) {
                        return Err(RepairError::new(
                            "bad_arguments",
                            format!("\"{key}\" must contain only values of the declared type"),
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn matches_type(value: &Value, schema: &Value) -> bool {
    match schema.get("type").and_then(|t| t.as_str()) {
        Some("string") => value.is_string(),
        Some("integer") => value.is_i64() || value.is_u64(),
        Some("number") => value.is_number(),
        Some("boolean") => value.is_boolean(),
        Some("array") => value.is_array(),
        Some("object") => value.is_object(),
        _ => true,
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn example_for(tool: &str) -> String {
    let example = match tool {
        "read" => json!({"path": "src/main.rs"}),
        "write" => json!({"path": "src/main.rs", "content": "..."}),
        "edit" => json!({"path": "src/main.rs", "old": "exact text", "new": "replacement"}),
        "patch" => {
            json!({"path": "src/main.rs", "anchor": "exact text", "replacement": "new text"})
        }
        "bash" => json!({"command": "cargo test"}),
        "grep" => json!({"pattern": "fn main"}),
        "glob" => json!({"pattern": "src/**/*.rs"}),
        "todo" => json!({"action": "plan", "tasks": ["first task", "second task"]}),
        "verify" => json!({"step": "test"}),
        "recall" => json!({"artifact": "out-1", "start": 1, "end": 120}),
        "thread" => json!({"action": "split", "title": "new thread", "objective": "..."}),
        "bg" => json!({"action": "start", "command": "cargo watch -x test"}),
        "lsp" => json!({"action": "outline", "path": "src/main.rs"}),
        "test" => json!({"path": "tests/parser.rs"}),
        "review" => json!({}),
        "finding" => {
            json!({"action": "add", "title": "...", "severity": "high", "target": "...", "evidence": "..."})
        }
        "surface" => json!({"action": "list"}),
        "evidence" => json!({"action": "list"}),
        "detect" => json!({"action": "run", "rules": "rules/", "target": "logs/"}),
        "attack_path" => json!({"action": "summary"}),
        "repo_map" => json!({"path": "src"}),
        "undo" => json!({"path": "src/main.rs"}),
        "iocs" => json!({"text": "http://example.test"}),
        _ => json!({}),
    };
    serde_json::to_string(&example).unwrap_or_else(|_| "{}".into())
}

fn reject_noops(tool: &str, value: &Value) -> Result<(), RepairError> {
    match tool {
        "edit" => {
            let old = value.get("old").and_then(|v| v.as_str()).unwrap_or("");
            let new = value.get("new").and_then(|v| v.as_str()).unwrap_or("");
            if old.is_empty() {
                return Err(RepairError::new(
                    "empty_old",
                    "edit needs the exact existing text in \"old\"; for a new file or a full rewrite use write",
                ));
            }
            if old == new {
                return Err(RepairError::new(
                    "noop_edit",
                    "edit old and new are identical; change \"new\" to the intended replacement",
                ));
            }
        }
        "patch" => {
            let anchor = value.get("anchor").and_then(|v| v.as_str()).unwrap_or("");
            let replacement = value
                .get("replacement")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if anchor.is_empty() {
                return Err(RepairError::new(
                    "empty_anchor",
                    "patch needs the exact anchor text; use write for new files",
                ));
            }
            if anchor == replacement {
                return Err(RepairError::new(
                    "noop_patch",
                    "patch anchor and replacement are identical; change the replacement",
                ));
            }
        }
        "write" => {
            if let Some(content) = value.get("content").and_then(|v| v.as_str())
                && content.is_empty()
                && value.get("allow_empty").and_then(|v| v.as_bool()) != Some(true)
            {
                return Err(RepairError::new(
                    "empty_content",
                    "write content is empty; to truncate a file intentionally pass \"allow_empty\": true",
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

/// Tool names other harnesses use, mapped to ours. Only names whose argument
/// shape the repair aliases can translate are routed; anything else gets a
/// suggestion instead.
const TOOL_ALIASES: &[(&str, &str)] = &[
    ("read_file", "read"),
    ("view_file", "read"),
    ("open_file", "read"),
    ("view", "read"),
    ("cat", "read"),
    ("write_file", "write"),
    ("create_file", "write"),
    ("edit_file", "edit"),
    ("str_replace", "edit"),
    ("replace_in_file", "edit"),
    ("search_replace", "edit"),
    ("replace", "edit"),
    ("shell", "bash"),
    ("sh", "bash"),
    ("terminal", "bash"),
    ("run", "bash"),
    ("run_command", "bash"),
    ("run_shell_command", "bash"),
    ("run_terminal_cmd", "bash"),
    ("execute", "bash"),
    ("execute_command", "bash"),
    ("exec", "bash"),
    ("search", "grep"),
    ("grep_search", "grep"),
    ("search_files", "grep"),
    ("ripgrep", "grep"),
    ("rg", "grep"),
    ("find_files", "glob"),
    ("file_search", "glob"),
    ("find", "glob"),
];

/// How a model-supplied tool name was resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolName {
    /// The name is an active tool, possibly after case or prefix cleanup.
    Exact(String),
    /// The name is another harness's spelling of an active tool.
    Routed { from: String, to: String },
    /// No active tool fits; `suggestion` is the closest active name, if any.
    Unknown { suggestion: Option<String> },
}

/// Resolve a tool name against the active tools. Handles `functions.read`
/// and `default_api:read` prefixes some chat templates emit, case drift, and
/// common names from other harnesses.
pub fn resolve_tool_name(name: &str, active: &[&str]) -> ToolName {
    if active.contains(&name) {
        return ToolName::Exact(name.to_string());
    }
    let bare = name
        .trim()
        .rsplit(['.', ':', '/'])
        .next()
        .unwrap_or(name)
        .trim();
    let lower = bare.to_ascii_lowercase();
    if let Some(hit) = active.iter().find(|tool| tool.eq_ignore_ascii_case(bare)) {
        return ToolName::Exact((*hit).to_string());
    }
    let snake = lower.replace('-', "_");
    if let Some((_, to)) = TOOL_ALIASES
        .iter()
        .find(|(alias, to)| *alias == snake && active.contains(to))
    {
        return ToolName::Routed {
            from: name.to_string(),
            to: (*to).to_string(),
        };
    }
    let suggestion = active
        .iter()
        .map(|tool| (edit_distance(&lower, tool), *tool))
        .filter(|(distance, tool)| *distance <= 2.max(tool.len() / 3))
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, tool)| tool.to_string());
    ToolName::Unknown { suggestion }
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let current = row[j + 1];
            row[j + 1] = if ca == *cb {
                previous
            } else {
                1 + previous.min(row[j]).min(row[j + 1])
            };
            previous = current;
        }
    }
    row[b.len()]
}

/// Detect TTY-only invocations that would hang the harness.
pub fn interactive_command(command: &str) -> Option<&'static str> {
    let mut tokens = command.split_whitespace();
    let program = tokens.next()?.rsplit('/').next().unwrap_or("");
    let rest: Vec<&str> = tokens.collect();
    let editors = ["vim", "vi", "nvim", "nano", "emacs", "pico", "micro"];
    if editors.contains(&program) {
        return Some(match program {
            "vim" | "vi" | "nvim" => "vim",
            "nano" => "nano",
            "emacs" => "emacs",
            _ => "an editor",
        });
    }
    let pagers = ["less", "more", "most"];
    if pagers.contains(&program) {
        return Some("a pager");
    }
    let monitors = ["top", "htop", "btop", "watch"];
    if monitors.contains(&program) {
        return Some(match program {
            "watch" => "watch",
            _ => "top",
        });
    }
    if ["man", "info"].contains(&program) {
        return Some("a pager");
    }
    if ["tmux", "screen"].contains(&program) {
        return Some("a terminal multiplexer");
    }
    if ["psql", "mysql", "sqlite3", "redis-cli"].contains(&program)
        && !rest
            .iter()
            .any(|arg| *arg == "-c" || *arg == "--command" || *arg == "-e")
    {
        return Some("an interactive database shell");
    }
    if ["python", "python3", "node", "irb", "ghci", "php", "ruby"].contains(&program)
        && rest.is_empty()
    {
        return Some("a REPL");
    }
    if program == "ssh" && rest.len() <= 1 {
        return Some("ssh");
    }
    None
}

fn remedy_for(blocked: &str) -> &'static str {
    match blocked {
        "vim" | "nano" | "emacs" | "an editor" => "Use read, edit, write, or patch to change files",
        "a pager" => "Read the file with read, or pipe through head",
        "top" | "watch" => "Use `bg` for a long-running monitor, or bash with `ps`",
        "a terminal multiplexer" => "Run the underlying command directly, or use bg",
        "ssh" => "Use `ssh host 'command'` so the command runs non-interactively",
        "a REPL" => "Pass the script as an argument, for example `python script.py`",
        "an interactive database shell" => "Pass a query with -c, for example `psql -c 'select 1'`",
        _ => "Use a non-interactive command",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;

    fn schema_for(name: &str) -> Value {
        ToolRegistry::builtin()
            .get(name)
            .map(|tool| tool.schema())
            .expect("tool exists")
    }

    #[test]
    fn double_encoded_arguments_are_parsed() {
        let value = parse_raw("\"{\\\"path\\\":\\\"a.rs\\\"}\"").unwrap();
        assert_eq!(value["path"], "a.rs");
    }

    #[test]
    fn aliases_and_stringified_scalars_are_repaired() {
        let schema = schema_for("read");
        let value = repair(
            "read",
            r#"{"file":"a.rs","limit":"20","startLine":"3"}"#,
            &schema,
        )
        .unwrap();
        assert_eq!(value["path"], "a.rs");
        assert_eq!(value["limit"], 20);
        assert_eq!(value["start"], 3);
    }

    #[test]
    fn defaults_are_applied() {
        let schema = schema_for("grep");
        let value = repair("grep", r#"{"pattern":"x"}"#, &schema).unwrap();
        assert_eq!(value["max_matches"], 200);
        // read applies its own default so an explicit end is never clipped.
        let value = repair("read", r#"{"path":"a.rs","end":400}"#, &schema_for("read")).unwrap();
        assert!(value.get("limit").is_none());
    }

    #[test]
    fn foreign_argument_names_are_translated_per_tool() {
        let value = repair(
            "edit",
            r#"{"file_path":"a.rs","old_string":"a","new_string":"b","replaceAll":"true"}"#,
            &schema_for("edit"),
        )
        .unwrap();
        assert_eq!(
            value,
            json!({"path":"a.rs","old":"a","new":"b","replace_all":true})
        );
        let value = repair(
            "patch",
            r#"{"path":"a.rs","old":"a","new":"b"}"#,
            &schema_for("patch"),
        )
        .unwrap();
        assert_eq!(value["anchor"], "a");
        assert_eq!(value["replacement"], "b");
        // An alias never overwrites the canonical key the model also sent.
        let value = repair(
            "read",
            r#"{"path":"real.rs","file_path":"other.rs"}"#,
            &schema_for("read"),
        )
        .unwrap();
        assert_eq!(value["path"], "real.rs");
    }

    #[test]
    fn nulls_floats_and_numbers_are_tolerated() {
        let value = repair(
            "read",
            r#"{"path":"a.rs","start":null,"end":40.0}"#,
            &schema_for("read"),
        )
        .unwrap();
        assert!(value.get("start").is_none());
        assert_eq!(value["end"], 40);
        let value = repair("todo", r#"{"action":"done","task":2}"#, &schema_for("todo")).unwrap();
        assert_eq!(value["task"], "2");
    }

    #[test]
    fn enum_values_are_folded() {
        let todo = schema_for("todo");
        assert_eq!(
            repair("todo", r#"{"action":" Done "}"#, &todo).unwrap()["action"],
            "done"
        );
        assert_eq!(
            repair("todo", r#"{"action":"complete"}"#, &todo).unwrap()["action"],
            "done"
        );
        let value = repair(
            "finding",
            r#"{"action":"update","id":"f1","status":"False_Positive"}"#,
            &schema_for("finding"),
        )
        .unwrap();
        assert_eq!(value["status"], "false-positive");
        let err = repair("todo", r#"{"action":"explode"}"#, &todo).unwrap_err();
        assert!(err.message.contains("must be one of"), "{}", err.message);
    }

    #[test]
    fn arrays_are_recovered_from_strings() {
        let todo = schema_for("todo");
        let value = repair(
            "todo",
            r#"{"action":"plan","tasks":"[\"a\",\"b\"]"}"#,
            &todo,
        )
        .unwrap();
        assert_eq!(value["tasks"], json!(["a", "b"]));
        let value = repair(
            "todo",
            r#"{"action":"plan","tasks":"1. Read parser\n2) Fix bug\n- Run tests\n"}"#,
            &todo,
        )
        .unwrap();
        assert_eq!(
            value["tasks"],
            json!(["Read parser", "Fix bug", "Run tests"])
        );
    }

    #[test]
    fn obvious_actions_are_inferred() {
        let value = repair("todo", r#"{"tasks":["a"]}"#, &schema_for("todo")).unwrap();
        assert_eq!(value["action"], "plan");
        let value = repair("bg", r#"{"command":"npm run dev"}"#, &schema_for("bg")).unwrap();
        assert_eq!(value["action"], "start");
    }

    #[test]
    fn dropped_keys_are_reported() {
        let schema = schema_for("read");
        let repaired = repair_with_notes("read", r#"{"path":"a.rs","bogus":1}"#, &schema).unwrap();
        assert_eq!(repaired.ignored, vec!["bogus".to_string()]);
        let note = repaired.note("read", &schema).unwrap();
        assert!(note.contains("ignored unknown argument(s) bogus"), "{note}");
        assert!(note.contains("path"), "{note}");
        // When a dropped key explains a missing argument, the error says so.
        let err = repair("grep", r#"{"needle":"x"}"#, &schema_for("grep")).unwrap_err();
        assert_eq!(err.code, "missing_argument");
        assert!(
            err.message.contains("ignored unknown argument(s) needle"),
            "{}",
            err.message
        );
    }

    #[test]
    fn tool_names_are_resolved() {
        let active = ["read", "edit", "bash", "grep"];
        assert_eq!(
            resolve_tool_name("read", &active),
            ToolName::Exact("read".into())
        );
        assert_eq!(
            resolve_tool_name("functions.read", &active),
            ToolName::Exact("read".into())
        );
        assert_eq!(
            resolve_tool_name("default_api:READ", &active),
            ToolName::Exact("read".into())
        );
        assert_eq!(
            resolve_tool_name("read_file", &active),
            ToolName::Routed {
                from: "read_file".into(),
                to: "read".into()
            }
        );
        assert_eq!(
            resolve_tool_name("run-shell-command", &active),
            ToolName::Routed {
                from: "run-shell-command".into(),
                to: "bash".into()
            }
        );
        // Routing only targets active tools.
        assert_eq!(
            resolve_tool_name("write_file", &active),
            ToolName::Unknown { suggestion: None }
        );
        assert_eq!(
            resolve_tool_name("gerp", &active),
            ToolName::Unknown {
                suggestion: Some("grep".into())
            }
        );
    }

    #[test]
    fn unknown_keys_are_dropped() {
        let schema = schema_for("read");
        let value = repair("read", r#"{"path":"a.rs","bogus":1}"#, &schema).unwrap();
        assert!(value.get("bogus").is_none());
    }

    #[test]
    fn missing_required_names_the_call() {
        let schema = schema_for("edit");
        let err = repair("edit", r#"{"path":"a.rs"}"#, &schema).unwrap_err();
        assert_eq!(err.code, "missing_argument");
        assert!(err.message.contains("\"old\""));
    }

    #[test]
    fn noop_edits_are_rejected() {
        let schema = schema_for("edit");
        let err = repair(
            "edit",
            r#"{"path":"a.rs","old":"same","new":"same"}"#,
            &schema,
        )
        .unwrap_err();
        assert_eq!(err.code, "noop_edit");
    }

    #[test]
    fn interactive_commands_are_rejected() {
        assert!(interactive_command("vim src/main.rs").is_some());
        assert!(interactive_command("ssh host").is_some());
        assert!(interactive_command("ssh host 'uptime'").is_none());
        assert!(interactive_command("python").is_some());
        assert!(interactive_command("python script.py").is_none());
        assert!(interactive_command("less README.md").is_some());
        assert!(interactive_command("cargo test").is_none());
        let schema = schema_for("bash");
        let err = repair("bash", r#"{"command":"vim a.rs"}"#, &schema).unwrap_err();
        assert_eq!(err.code, "interactive_command");
    }

    #[test]
    fn canonical_json_sorts_keys() {
        let a = json!({"b":1,"a":{"d":2,"c":3}});
        let b = json!({"a":{"c":3,"d":2},"b":1});
        assert_eq!(canonical_json(&a), canonical_json(&b));
    }
}
