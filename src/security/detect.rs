//! Detection-as-code: a deterministic Sigma subset for local logs and samples.
//!
//! Supported rule shape:
//!
//! ```yaml
//! title: Suspicious shell
//! id: shell-curl
//! level: high
//! detection:
//!   selection:
//!     message|contains: "curl "
//!   condition: selection
//! ```
//!
//! Selections are mappings (all fields must match) or lists of mappings (any
//! mapping matches). Keywords are matched against the raw line. Field
//! modifiers: `contains`, `startswith`, `endswith`, `re`, `exists`, `all`,
//! `cased`. Conditions support `and`, `or`, `not`, parentheses, `1 of name*`,
//! `all of name*`, and `them`. This is intentionally a subset, not a full Sigma
//! implementation; `validate` reports unsupported syntax instead of guessing.

use std::collections::BTreeMap;
use std::path::Path;

use ignore::WalkBuilder;
use regex::Regex;
use serde::Deserialize;
use serde_yaml::Value as Yaml;

use crate::security::findings::Severity;

const MAX_RULE_FILES: usize = 300;
const MAX_HITS: usize = 500;
const MAX_TARGET_FILES: usize = 500;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LINES: usize = 250_000;
const MAX_MESSAGE_CHARS: usize = 300;

#[derive(Debug, Clone)]
pub struct CompiledRule {
    pub id: String,
    pub title: String,
    pub description: String,
    pub level: Severity,
    pub source: String,
    selections: BTreeMap<String, Selection>,
    condition: Condition,
}

#[derive(Debug, Clone)]
pub struct RuleSet {
    pub rules: Vec<CompiledRule>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
enum Selection {
    All(Vec<FieldMatch>),
    AnyOf(Vec<Vec<FieldMatch>>),
    Keywords(Vec<String>),
}

#[derive(Debug, Clone)]
struct FieldMatch {
    field: String,
    kind: MatchKind,
    values: Vec<String>,
    all: bool,
    cased: bool,
}

#[derive(Debug, Clone)]
enum MatchKind {
    Equals,
    Contains,
    StartsWith,
    EndsWith,
    Regex(Regex),
    Exists(bool),
}

#[derive(Debug, Clone)]
enum Condition {
    Ident(String),
    Not(Box<Condition>),
    And(Box<Condition>, Box<Condition>),
    Or(Box<Condition>, Box<Condition>),
    Count {
        all: bool,
        at_least: usize,
        pattern: String,
    },
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub rule_id: String,
    pub title: String,
    pub level: Severity,
    pub source: String,
    pub line: usize,
    pub matched: Vec<String>,
    pub message: String,
}

impl RuleSet {
    pub fn load(path: &Path) -> Self {
        let mut set = Self {
            rules: Vec::new(),
            errors: Vec::new(),
        };
        let mut files = Vec::new();
        if path.is_file() {
            files.push(path.to_path_buf());
        } else if path.is_dir() {
            let mut builder = WalkBuilder::new(path);
            builder
                .hidden(true)
                .git_ignore(true)
                .parents(true)
                .max_filesize(Some(2 * 1024 * 1024));
            for entry in builder.build().flatten() {
                if files.len() >= MAX_RULE_FILES {
                    set.errors
                        .push(format!("rule directory capped at {MAX_RULE_FILES} files"));
                    break;
                }
                if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                    continue;
                }
                let extension = entry
                    .path()
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if extension == "yml" || extension == "yaml" {
                    files.push(entry.path().to_path_buf());
                }
            }
        } else {
            set.errors
                .push(format!("rule path {} does not exist", path.display()));
            return set;
        }
        for file in files {
            let raw = match std::fs::read_to_string(&file) {
                Ok(raw) => raw,
                Err(error) => {
                    set.errors
                        .push(format!("{}: cannot read: {error}", file.display()));
                    continue;
                }
            };
            let documents = serde_yaml::Deserializer::from_str(&raw);
            for (index, document) in documents.enumerate() {
                let value = match Yaml::deserialize(document) {
                    Ok(value) => value,
                    Err(error) => {
                        set.errors
                            .push(format!("{}:{}: {error}", file.display(), index + 1));
                        continue;
                    }
                };
                if value.is_null() {
                    continue;
                }
                match compile_rule(value, &file) {
                    Ok(rule) => set.rules.push(rule),
                    Err(error) => set.errors.push(format!("{}: {error}", file.display())),
                }
            }
        }
        set
    }
}

/// Match every valid rule against a target file or directory.
pub fn run(
    rules: &[CompiledRule],
    target: &Path,
    cwd: &Path,
    limit: usize,
) -> Result<Vec<Hit>, String> {
    let limit = limit.clamp(1, MAX_HITS);
    let mut files = Vec::new();
    if target.is_file() {
        files.push(target.to_path_buf());
    } else if target.is_dir() {
        let mut builder = WalkBuilder::new(target);
        builder
            .hidden(false)
            .git_ignore(true)
            .parents(true)
            .max_filesize(Some(MAX_FILE_BYTES))
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                !matches!(
                    name.as_ref(),
                    ".git"
                        | ".hg"
                        | ".svn"
                        | "target"
                        | "node_modules"
                        | "dist"
                        | "build"
                        | "__pycache__"
                )
            });
        for entry in builder.build().flatten() {
            if files.len() >= MAX_TARGET_FILES {
                break;
            }
            if entry.file_type().is_some_and(|kind| kind.is_file()) {
                files.push(entry.path().to_path_buf());
            }
        }
    } else {
        return Err(format!("target {} does not exist", target.display()));
    }

    let mut hits = Vec::new();
    let mut lines_seen = 0usize;
    'files: for file in files {
        let Ok(bytes) = std::fs::read(&file) else {
            continue;
        };
        if bytes.iter().take(4096).any(|byte| *byte == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let source = file
            .strip_prefix(cwd)
            .unwrap_or(&file)
            .display()
            .to_string();
        for (index, line) in text.lines().enumerate() {
            lines_seen += 1;
            if lines_seen > MAX_LINES {
                break 'files;
            }
            if line.trim().is_empty() {
                continue;
            }
            let record = match serde_json::from_str::<serde_json::Value>(line) {
                Ok(value) if value.is_object() => value,
                _ => serde_json::json!({"message": line, "line": line}),
            };
            for rule in rules {
                if let Some(matched) = rule.matches(&record, line) {
                    hits.push(Hit {
                        rule_id: rule.id.clone(),
                        title: rule.title.clone(),
                        level: rule.level,
                        source: source.clone(),
                        line: index + 1,
                        matched,
                        message: clip(line.trim(), MAX_MESSAGE_CHARS),
                    });
                    if hits.len() >= limit {
                        break 'files;
                    }
                }
            }
        }
    }
    Ok(hits)
}

impl CompiledRule {
    fn matches(&self, record: &serde_json::Value, raw: &str) -> Option<Vec<String>> {
        let mut matched = BTreeMap::new();
        let mut details = Vec::new();
        for (name, selection) in &self.selections {
            let hit = selection.matches(record, raw);
            if let Some(fields) = &hit {
                details.extend(fields.iter().cloned());
            }
            matched.insert(name.clone(), hit.is_some());
        }
        if self.condition.eval(&matched) {
            details.sort();
            details.dedup();
            details.truncate(8);
            Some(details)
        } else {
            None
        }
    }
}

impl Selection {
    fn matches(&self, record: &serde_json::Value, raw: &str) -> Option<Vec<String>> {
        match self {
            Selection::All(fields) => {
                let mut matched = Vec::new();
                for field in fields {
                    matched.extend(field.matches(record)?);
                }
                Some(matched)
            }
            Selection::AnyOf(maps) => maps.iter().find_map(|fields| {
                let mut matched = Vec::new();
                for field in fields {
                    matched.extend(field.matches(record)?);
                }
                Some(matched)
            }),
            Selection::Keywords(keywords) => {
                let lower = raw.to_lowercase();
                keywords
                    .iter()
                    .find(|keyword| lower.contains(&keyword.to_lowercase()))
                    .map(|_| vec!["keyword".to_string()])
            }
        }
    }
}

impl FieldMatch {
    fn matches(&self, record: &serde_json::Value) -> Option<Vec<String>> {
        let values = values_at(record, &self.field);
        if let MatchKind::Exists(expected) = &self.kind {
            let present = !values.is_empty();
            return (present == *expected).then(|| vec![self.field.clone()]);
        }
        if values.is_empty() {
            return None;
        }
        let matched = if self.all {
            self.values.iter().all(|expected| {
                values
                    .iter()
                    .any(|value| self.matches_value(value, expected))
            })
        } else {
            self.values.iter().any(|expected| {
                values
                    .iter()
                    .any(|value| self.matches_value(value, expected))
            })
        };
        matched.then(|| vec![self.field.clone()])
    }

    fn matches_value(&self, value: &str, expected: &str) -> bool {
        if let MatchKind::Regex(regex) = &self.kind {
            return regex.is_match(value);
        }
        let (value, expected) = if self.cased {
            (value.to_string(), expected.to_string())
        } else {
            (value.to_lowercase(), expected.to_lowercase())
        };
        match &self.kind {
            MatchKind::Equals => value == expected,
            MatchKind::Contains => value.contains(&expected),
            MatchKind::StartsWith => value.starts_with(&expected),
            MatchKind::EndsWith => value.ends_with(&expected),
            MatchKind::Regex(_) | MatchKind::Exists(_) => true,
        }
    }
}

impl Condition {
    fn eval(&self, matched: &BTreeMap<String, bool>) -> bool {
        match self {
            Condition::Ident(name) => selection_true(matched, name),
            Condition::Not(inner) => !inner.eval(matched),
            Condition::And(left, right) => left.eval(matched) && right.eval(matched),
            Condition::Or(left, right) => left.eval(matched) || right.eval(matched),
            Condition::Count {
                all,
                at_least,
                pattern,
            } => {
                let names: Vec<&String> = if pattern == "them" {
                    matched.keys().collect()
                } else {
                    matched
                        .keys()
                        .filter(|name| wildcard_match(pattern, name))
                        .collect()
                };
                if names.is_empty() {
                    return false;
                }
                let hits = names
                    .iter()
                    .filter(|name| matched.get(name.as_str()).copied().unwrap_or(false))
                    .count();
                if *all {
                    hits == names.len()
                } else {
                    hits >= *at_least
                }
            }
        }
    }
}

fn selection_true(matched: &BTreeMap<String, bool>, pattern: &str) -> bool {
    if pattern.contains('*') {
        matched
            .iter()
            .filter(|(name, _)| wildcard_match(pattern, name))
            .any(|(_, hit)| *hit)
    } else {
        matched.get(pattern).copied().unwrap_or(false)
    }
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else {
        return false;
    };
    if !text.starts_with(first) {
        return false;
    }
    let mut rest = &text[first.len()..];
    for part in parts {
        if part.is_empty() {
            continue;
        }
        let Some(index) = rest.find(part) else {
            return false;
        };
        rest = &rest[index + part.len()..];
    }
    true
}

fn compile_rule(value: Yaml, path: &Path) -> Result<CompiledRule, String> {
    let mapping = value
        .as_mapping()
        .ok_or_else(|| "rule is not a YAML mapping".to_string())?;
    let title = yaml_string(yaml_get(mapping, "title"))
        .filter(|title| !title.trim().is_empty())
        .ok_or_else(|| "rule has no title".to_string())?;
    let id = yaml_string(yaml_get(mapping, "id"))
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| slug(&title));
    let description = yaml_string(yaml_get(mapping, "description")).unwrap_or_default();
    let level = match yaml_string(yaml_get(mapping, "level")) {
        Some(level) => level
            .parse::<Severity>()
            .map_err(|error| format!("invalid level: {error}"))?,
        None => Severity::default(),
    };
    let detection = yaml_get(mapping, "detection")
        .and_then(Yaml::as_mapping)
        .ok_or_else(|| "rule has no detection mapping".to_string())?;
    let condition = yaml_string(yaml_get(detection, "condition"))
        .ok_or_else(|| "detection.condition is required".to_string())?;
    let condition = parse_condition(&condition)?;
    let mut selections = BTreeMap::new();
    for (key, value) in detection {
        let Some(name) = key.as_str() else {
            continue;
        };
        if name == "condition" || name == "timeframe" {
            continue;
        }
        let selection =
            parse_selection(value).map_err(|error| format!("selection {name:?}: {error}"))?;
        selections.insert(name.to_string(), selection);
    }
    if selections.is_empty() {
        return Err("detection has no selections".into());
    }
    validate_condition(&condition, &selections)?;
    Ok(CompiledRule {
        id,
        title,
        description,
        level,
        source: path.display().to_string(),
        selections,
        condition,
    })
}

fn parse_selection(value: &Yaml) -> Result<Selection, String> {
    match value {
        Yaml::Mapping(mapping) => Ok(Selection::All(parse_field_map(mapping)?)),
        Yaml::Sequence(sequence) => {
            if sequence.iter().all(|item| item.is_mapping()) {
                let mut maps = Vec::new();
                for item in sequence {
                    let mapping = item
                        .as_mapping()
                        .ok_or_else(|| "selection list contains a non-mapping".to_string())?;
                    maps.push(parse_field_map(mapping)?);
                }
                Ok(Selection::AnyOf(maps))
            } else if sequence.iter().all(|item| scalar_string(item).is_some()) {
                Ok(Selection::Keywords(
                    sequence
                        .iter()
                        .filter_map(scalar_string)
                        .collect::<Vec<_>>(),
                ))
            } else {
                Err("selection list mixes mappings and scalars".into())
            }
        }
        Yaml::String(value) => Ok(Selection::Keywords(vec![value.clone()])),
        Yaml::Number(number) => Ok(Selection::Keywords(vec![number.to_string()])),
        _ => Err("selection must be a mapping, list, or string".into()),
    }
}

fn parse_field_map(mapping: &serde_yaml::Mapping) -> Result<Vec<FieldMatch>, String> {
    let mut fields = Vec::new();
    for (key, value) in mapping {
        let key = key
            .as_str()
            .ok_or_else(|| "field name is not a string".to_string())?;
        fields.push(parse_field(key, value)?);
    }
    if fields.is_empty() {
        return Err("field selection is empty".into());
    }
    Ok(fields)
}

fn parse_field(key: &str, value: &Yaml) -> Result<FieldMatch, String> {
    let mut parts = key.split('|');
    let field = parts.next().unwrap_or("").trim();
    if field.is_empty() {
        return Err("field name is empty".into());
    }
    let mut kind = MatchKind::Equals;
    let mut all = false;
    let mut cased = false;
    let mut kind_set = false;
    for modifier in parts {
        match modifier.to_ascii_lowercase().as_str() {
            "contains" => {
                set_kind(&mut kind, MatchKind::Contains, &mut kind_set)?;
            }
            "startswith" => {
                set_kind(&mut kind, MatchKind::StartsWith, &mut kind_set)?;
            }
            "endswith" => {
                set_kind(&mut kind, MatchKind::EndsWith, &mut kind_set)?;
            }
            "re" => {
                let Yaml::String(pattern) = value else {
                    return Err("|re needs a string pattern".into());
                };
                let regex = Regex::new(pattern)
                    .map_err(|error| format!("invalid regex {pattern:?}: {error}"))?;
                set_kind(&mut kind, MatchKind::Regex(regex), &mut kind_set)?;
            }
            "exists" => {
                if kind_set {
                    return Err("|exists cannot be combined with another matcher".into());
                }
                let expected = match value {
                    Yaml::Bool(expected) => *expected,
                    _ => return Err("|exists needs true or false".into()),
                };
                kind = MatchKind::Exists(expected);
                kind_set = true;
            }
            "all" => all = true,
            "cased" => cased = true,
            other => return Err(format!("unsupported modifier |{other}")),
        }
    }
    let values = scalar_values(value);
    if values.is_empty() && !matches!(kind, MatchKind::Exists(_)) {
        return Err("field value is empty".into());
    }
    Ok(FieldMatch {
        field: field.to_string(),
        kind,
        values,
        all,
        cased,
    })
}

fn set_kind(kind: &mut MatchKind, next: MatchKind, set: &mut bool) -> Result<(), String> {
    if *set {
        return Err("more than one match modifier on a field".into());
    }
    *kind = next;
    *set = true;
    Ok(())
}

fn scalar_values(value: &Yaml) -> Vec<String> {
    match value {
        Yaml::Sequence(sequence) => sequence.iter().filter_map(scalar_string).collect(),
        _ => scalar_string(value).into_iter().collect(),
    }
}

fn scalar_string(value: &Yaml) -> Option<String> {
    match value {
        Yaml::String(value) => Some(value.clone()),
        Yaml::Number(value) => Some(value.to_string()),
        Yaml::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn yaml_string(value: Option<&Yaml>) -> Option<String> {
    value.and_then(|value| value.as_str()).map(str::to_string)
}

fn yaml_get<'a>(mapping: &'a serde_yaml::Mapping, key: &str) -> Option<&'a Yaml> {
    mapping.get(Yaml::String(key.to_string()))
}

fn values_at(record: &serde_json::Value, path: &str) -> Vec<String> {
    let mut current = vec![record];
    for part in path.split('.') {
        let mut next = Vec::new();
        for value in current {
            match value {
                serde_json::Value::Object(map) => {
                    if let Some(value) = map.get(part) {
                        next.push(value);
                    }
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        if let serde_json::Value::Object(map) = item
                            && let Some(value) = map.get(part)
                        {
                            next.push(value);
                        }
                    }
                }
                _ => {}
            }
        }
        current = next;
        if current.is_empty() {
            return Vec::new();
        }
    }
    let mut out = Vec::new();
    for value in current {
        flatten_value(value, &mut out);
    }
    out
}

fn flatten_value(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(value) => out.push(value.clone()),
        serde_json::Value::Number(value) => out.push(value.to_string()),
        serde_json::Value::Bool(value) => out.push(value.to_string()),
        serde_json::Value::Array(items) => {
            for item in items {
                flatten_value(item, out);
            }
        }
        serde_json::Value::Object(_) => {
            if let Ok(text) = serde_json::to_string(value) {
                out.push(text);
            }
        }
        serde_json::Value::Null => {}
    }
}

fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

fn slug(title: &str) -> String {
    let mut slug = String::new();
    for character in title.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if slug.ends_with('-') {
            continue;
        } else {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "detection".into()
    } else {
        slug.to_string()
    }
}

// --- condition parser -------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Ident(String),
    Number(usize),
    And,
    Or,
    Not,
    Of,
    All,
    Any,
    Them,
    Open,
    Close,
}

fn parse_condition(input: &str) -> Result<Condition, String> {
    let tokens = tokenize_condition(input)?;
    if tokens.is_empty() {
        return Err("detection.condition is empty".into());
    }
    let mut parser = ConditionParser { tokens, pos: 0 };
    let condition = parser.parse_or()?;
    if parser.pos != parser.tokens.len() {
        return Err(format!(
            "unexpected token in condition: {:?}",
            parser.tokens[parser.pos]
        ));
    }
    Ok(condition)
}

struct ConditionParser {
    tokens: Vec<Token>,
    pos: usize,
}

impl ConditionParser {
    fn parse_or(&mut self) -> Result<Condition, String> {
        let mut left = self.parse_and()?;
        while self.take(Token::Or) {
            let right = self.parse_and()?;
            left = Condition::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Condition, String> {
        let mut left = self.parse_not()?;
        while self.take(Token::And) {
            let right = self.parse_not()?;
            left = Condition::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Condition, String> {
        if self.take(Token::Not) {
            return Ok(Condition::Not(Box::new(self.parse_not()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Condition, String> {
        let Some(token) = self.next() else {
            return Err("condition ends unexpectedly".into());
        };
        match token {
            Token::Open => {
                let condition = self.parse_or()?;
                if !self.take(Token::Close) {
                    return Err("condition is missing ')'".into());
                }
                Ok(condition)
            }
            Token::Number(at_least) => {
                self.expect(Token::Of)?;
                let pattern = self.pattern()?;
                Ok(Condition::Count {
                    all: false,
                    at_least,
                    pattern,
                })
            }
            Token::Any => {
                self.expect(Token::Of)?;
                Ok(Condition::Count {
                    all: false,
                    at_least: 1,
                    pattern: self.pattern()?,
                })
            }
            Token::All => {
                self.expect(Token::Of)?;
                Ok(Condition::Count {
                    all: true,
                    at_least: usize::MAX,
                    pattern: self.pattern()?,
                })
            }
            Token::Ident(name) => Ok(Condition::Ident(name)),
            other => Err(format!("unexpected token in condition: {other:?}")),
        }
    }

    fn pattern(&mut self) -> Result<String, String> {
        match self.next() {
            Some(Token::Ident(pattern)) => Ok(pattern),
            Some(Token::Them) => Ok("them".into()),
            other => Err(format!("expected a selection name, got {other:?}")),
        }
    }

    fn expect(&mut self, expected: Token) -> Result<(), String> {
        if self.take(expected.clone()) {
            Ok(())
        } else {
            Err(format!("expected {expected:?} in condition"))
        }
    }

    fn take(&mut self, expected: Token) -> bool {
        if self.tokens.get(self.pos) == Some(&expected) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }
}

fn tokenize_condition(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '(' => tokens.push(Token::Open),
            ')' => tokens.push(Token::Close),
            character if character.is_whitespace() => {}
            character if character.is_ascii_digit() => {
                let mut number = String::from(character);
                while let Some(next) = chars.peek().copied() {
                    if !next.is_ascii_digit() {
                        break;
                    }
                    number.push(next);
                    chars.next();
                }
                tokens.push(Token::Number(
                    number
                        .parse::<usize>()
                        .map_err(|error| format!("invalid count {number:?}: {error}"))?,
                ));
            }
            character
                if character.is_alphanumeric() || matches!(character, '_' | '-' | '*' | '.') =>
            {
                let mut word = String::from(character);
                while let Some(next) = chars.peek().copied() {
                    if !(next.is_alphanumeric() || matches!(next, '_' | '-' | '*' | '.')) {
                        break;
                    }
                    word.push(next);
                    chars.next();
                }
                tokens.push(match word.to_ascii_lowercase().as_str() {
                    "and" => Token::And,
                    "or" => Token::Or,
                    "not" => Token::Not,
                    "of" => Token::Of,
                    "all" => Token::All,
                    "any" => Token::Any,
                    "them" => Token::Them,
                    _ => Token::Ident(word),
                });
            }
            '|' => return Err("condition pipes are not supported; use and/or/not".into()),
            other => return Err(format!("unexpected character {other:?} in condition")),
        }
    }
    Ok(tokens)
}

fn validate_condition(
    condition: &Condition,
    selections: &BTreeMap<String, Selection>,
) -> Result<(), String> {
    match condition {
        Condition::Ident(name) => {
            if name.contains('*') {
                if !selections
                    .keys()
                    .any(|selection| wildcard_match(name, selection))
                {
                    return Err(format!(
                        "condition references no selection matching {name:?}"
                    ));
                }
            } else if !selections.contains_key(name) {
                return Err(format!("condition references unknown selection {name:?}"));
            }
        }
        Condition::Not(inner) => validate_condition(inner, selections)?,
        Condition::And(left, right) | Condition::Or(left, right) => {
            validate_condition(left, selections)?;
            validate_condition(right, selections)?;
        }
        Condition::Count { pattern, .. } => {
            if pattern != "them"
                && !selections
                    .keys()
                    .any(|selection| wildcard_match(pattern, selection))
            {
                return Err(format!(
                    "condition references no selection matching {pattern:?}"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(yaml: &str) -> CompiledRule {
        let value: Yaml = serde_yaml::from_str(yaml).unwrap();
        compile_rule(value, Path::new("rule.yml")).unwrap()
    }

    #[test]
    fn matches_contains_and_list_values() {
        let rule = rule(
            "title: Curl shell\nlevel: high\ndetection:\n  selection:\n    message|contains:\n      - curl\n      - wget\n  condition: selection\n",
        );
        assert!(
            rule.matches(&json!({"message": "curl http://x"}), "")
                .is_some()
        );
        assert!(
            rule.matches(&json!({"message": "wget http://x"}), "")
                .is_some()
        );
        assert!(rule.matches(&json!({"message": "ls"}), "").is_none());
    }

    #[test]
    fn supports_condition_sets_and_not() {
        let rule = rule(
            "title: t\nlevel: low\ndetection:\n  selection:\n    message|contains: evil\n  filter:\n    user: root\n  condition: selection and not filter\n",
        );
        assert!(
            rule.matches(&json!({"message": "evil", "user": "alice"}), "")
                .is_some()
        );
        assert!(
            rule.matches(&json!({"message": "evil", "user": "root"}), "")
                .is_none()
        );
    }

    #[test]
    fn supports_quantified_conditions() {
        let all_rule = rule(
            "title: t\ndetection:\n  one:\n    a: 1\n  two:\n    b: 2\n  condition: all of them\n",
        );
        assert!(all_rule.matches(&json!({"a": 1, "b": 2}), "").is_some());
        assert!(all_rule.matches(&json!({"a": 1}), "").is_none());
        let any_rule = rule(
            "title: t\ndetection:\n  one:\n    a: 1\n  two:\n    b: 2\n  condition: 1 of them\n",
        );
        assert!(any_rule.matches(&json!({"a": 1}), "").is_some());
    }

    #[test]
    fn keyword_selections_match_raw_text() {
        let rule =
            rule("title: t\ndetection:\n  keywords:\n    - evil.exe\n  condition: keywords\n");
        assert!(rule.matches(&json!({}), "cmd /c evil.exe").is_some());
    }

    #[test]
    fn invalid_condition_is_reported() {
        let value: Yaml = serde_yaml::from_str(
            "title: t\ndetection:\n  selection:\n    a: 1\n  condition: missing\n",
        )
        .unwrap();
        assert!(compile_rule(value, Path::new("r.yml")).is_err());
    }

    #[test]
    fn loads_multiple_documents_and_text_targets() {
        let dir = tempfile::tempdir().unwrap();
        let rules = dir.path().join("rules");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(
            rules.join("one.yml"),
            "title: One\ndetection:\n  selection:\n    message|contains: evil\n  condition: selection\n---\ntitle: Two\ndetection:\n  selection:\n    message|contains: bad\n  condition: selection\n",
        )
        .unwrap();
        let set = RuleSet::load(&rules);
        assert_eq!(set.rules.len(), 2, "{:?}", set.errors);
        let target = dir.path().join("sample.log");
        std::fs::write(&target, "hello\nevil happened\n").unwrap();
        let hits = run(&set.rules, &target, dir.path(), 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 2);
        assert_eq!(hits[0].source, "sample.log");
    }
}
