//! YAML frontmatter parsing for `SKILL.md`.
//!
//! The native harness format and the Agent Skills specification share the same
//! shape: a YAML mapping between `---` fences, then Markdown. Parsing through
//! `serde_yaml` handles folded descriptions and lists, and every field the
//! harness does not use is kept verbatim so a skill can round-trip through
//! discovery, lint, and install without losing metadata.

use std::collections::BTreeMap;

use serde_yaml::Value;

/// Fields the harness interprets. Everything else is preserved as unknown
/// frontmatter and handed back unchanged.
pub const KNOWN_FIELDS: &[&str] = &["name", "description", "when", "allowed-tools", "packs"];

#[derive(Debug, Clone, Default)]
pub struct Frontmatter {
    pub fields: BTreeMap<String, Value>,
    pub body: String,
    pub present: bool,
}

impl Frontmatter {
    /// Parse one Markdown document. Returns an error only when a frontmatter
    /// block exists but is not a YAML mapping; an unterminated fence is treated
    /// as body text, which keeps older native skills loadable.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim_start_matches('\u{feff}');
        let Some((front, body)) = split(trimmed) else {
            return Ok(Self {
                fields: BTreeMap::new(),
                body: trimmed.trim_start_matches(['\r', '\n']).to_string(),
                present: false,
            });
        };
        if front.trim().is_empty() {
            return Ok(Self {
                fields: BTreeMap::new(),
                body: body.trim_start_matches(['\r', '\n']).to_string(),
                present: true,
            });
        }
        let value: Value = serde_yaml::from_str(front)
            .map_err(|error| format!("frontmatter is not valid YAML: {error}"))?;
        let mapping = match value {
            Value::Mapping(mapping) => mapping,
            Value::Null => serde_yaml::Mapping::new(),
            _ => {
                return Err(
                    "frontmatter must be a YAML mapping of fields, for example `name: demo`"
                        .to_string(),
                );
            }
        };
        let mut fields = BTreeMap::new();
        for (key, value) in mapping {
            let key = match key {
                Value::String(key) => key,
                other => {
                    return Err(format!(
                        "frontmatter keys must be strings, found {}",
                        yaml_type(&other)
                    ));
                }
            };
            fields.insert(key, value);
        }
        Ok(Self {
            fields,
            body: body.trim_start_matches(['\r', '\n']).to_string(),
            present: true,
        })
    }

    /// A scalar rendered as text. Strings keep their text; numbers and booleans
    /// are stringified so a stray `name: 42` still gets a field-level error
    /// instead of being silently ignored.
    pub fn scalar(&self, key: &str) -> Option<String> {
        self.fields.get(key).and_then(scalar_value)
    }

    /// A whitespace/comma separated string or a YAML sequence.
    pub fn list(&self, key: &str) -> Vec<String> {
        let Some(value) = self.fields.get(key) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match value {
            Value::Sequence(items) => {
                for item in items {
                    if let Some(text) = scalar_value(item) {
                        push_list(&mut out, &text);
                    }
                }
            }
            other => {
                if let Some(text) = scalar_value(other) {
                    push_list(&mut out, &text);
                }
            }
        }
        out
    }

    pub fn truthy(&self, key: &str) -> bool {
        match self.fields.get(key) {
            Some(Value::Bool(value)) => *value,
            Some(Value::String(value)) => {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "true" | "yes" | "on" | "1"
                )
            }
            Some(Value::Number(value)) => value.as_i64() == Some(1),
            _ => false,
        }
    }

    /// Fields the harness does not interpret, in stable order.
    pub fn unknown(&self) -> BTreeMap<String, Value> {
        self.fields
            .iter()
            .filter(|(key, _)| !KNOWN_FIELDS.iter().any(|known| known == key))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }
}

fn push_list(out: &mut Vec<String>, text: &str) {
    for part in text.split([',', ' ', '\t', '\n']) {
        let part = part.trim();
        if !part.is_empty() && !out.iter().any(|item| item == part) {
            out.push(part.to_string());
        }
    }
}

fn scalar_value(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.trim().to_string()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Null => None,
        _ => None,
    }
}

fn yaml_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Sequence(_) => "a sequence",
        Value::Mapping(_) => "a mapping",
        Value::Tagged(_) => "a tagged value",
    }
}

/// Split `---\n...\n---\n` into (frontmatter, body). The closing fence is a
/// line that is exactly `---` or `...`.
fn split(raw: &str) -> Option<(&str, &str)> {
    let rest = raw.strip_prefix("---")?;
    // The opening fence must end the first line; `---foo` is body text.
    let first_break = rest.find('\n')?;
    if !rest[..first_break].trim().is_empty() {
        return None;
    }
    let after_open = &rest[first_break + 1..];
    let mut offset = 0;
    for line in after_open.split_inclusive('\n') {
        let text = line.trim_end_matches(['\r', '\n']);
        if text == "---" || text == "..." {
            return Some((&after_open[..offset], &after_open[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_folded_descriptions_and_lists() {
        let raw = "---\nname: demo\ndescription: >\n  One line\n  and another.\nallowed-tools:\n  - bash\n  - read\nmetadata:\n  upstream: x\n---\n\nBody\n";
        let front = Frontmatter::parse(raw).unwrap();
        assert_eq!(front.scalar("name").as_deref(), Some("demo"));
        assert_eq!(
            front.scalar("description").as_deref(),
            Some("One line and another.")
        );
        assert_eq!(front.list("allowed-tools"), ["bash", "read"]);
        assert_eq!(front.body.trim(), "Body");
        assert!(front.unknown().contains_key("metadata"));
    }

    #[test]
    fn unknown_fields_survive_and_bad_yaml_is_an_error() {
        let front = Frontmatter::parse("---\nname: x\nfuture: [1, 2]\n---\nbody").unwrap();
        assert_eq!(front.unknown().len(), 1);
        assert!(Frontmatter::parse("---\n- just\n- a list\n---\nbody").is_err());
        // No fence is body text, not an error.
        let plain = Frontmatter::parse("# Title\n\nText").unwrap();
        assert!(!plain.present);
        assert!(plain.body.contains("# Title"));
    }
}
