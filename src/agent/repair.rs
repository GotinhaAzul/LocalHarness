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

const ALIASES: &[(&str, &str)] = &[
    ("file", "path"),
    ("filepath", "path"),
    ("filename", "path"),
    ("cmd", "command"),
    ("oldText", "old"),
    ("old_text", "old"),
    ("newText", "new"),
    ("new_text", "new"),
    ("startLine", "start"),
    ("start_line", "start"),
    ("endLine", "end"),
    ("end_line", "end"),
    ("glob_pattern", "pattern"),
    ("maxResults", "max_matches"),
    ("max_results", "max_matches"),
    ("timeout_secs", "timeout"),
];

fn apply_aliases(map: &mut Map<String, Value>) {
    for (alias, canonical) in ALIASES {
        if canonical == alias {
            continue;
        }
        if let Some(value) = map.remove(*alias)
            && !map.contains_key(*canonical)
        {
            map.insert((*canonical).to_string(), value);
        }
    }
}

fn apply_defaults(tool: &str, map: &mut Map<String, Value>) {
    match tool {
        "read" => {
            map.entry("limit".to_string()).or_insert(json!(150));
        }
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
                "array" => {}
                _ => {}
            }
        }
    }
    value
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

/// Repair and validate tool arguments.
pub fn repair(tool: &str, raw: &str, schema: &Value) -> Result<Value, RepairError> {
    let mut value = parse_raw(raw)?;
    let Value::Object(map) = &mut value else {
        return Err(RepairError::new(
            "bad_arguments",
            "arguments must be an object",
        ));
    };
    apply_aliases(map);
    apply_defaults(tool, map);
    if let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) {
        for (key, property) in properties {
            if let Some(existing) = map.remove(key) {
                map.insert(key.clone(), coerce(existing, property));
            }
        }
    }
    drop_unknown(map, schema);

    validate(tool, &value, schema)?;
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
    Ok(value)
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
            let example = example_for(tool, &required);
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

fn example_for(tool: &str, required: &[&str]) -> String {
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
        "finding" => json!({"action": "add", "title": "...", "severity": "high", "target": "..."}),
        "iocs" => json!({"text": "http://example.test"}),
        _ => json!({}),
    };
    if required.is_empty() {
        return serde_json::to_string(&example).unwrap_or_else(|_| "{}".into());
    }
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
        let schema = schema_for("read");
        let value = repair("read", r#"{"path":"a.rs"}"#, &schema).unwrap();
        assert_eq!(value["limit"], 150);
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
