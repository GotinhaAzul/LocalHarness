//! Structured-output adapter.
//!
//! Structured output is not one API. llama.cpp accepts a GBNF `grammar`,
//! vLLM exposes `structured_outputs`, SGLang accepts an OpenAI-style
//! `response_format`, and KoboldCpp converts a schema with
//! `/api/extra/json_to_grammar`. MLX has no grammar backend.
//!
//! One schema subset is validated up front (unsupported keywords are rejected
//! by name), lowered to each engine, and validated again after parsing so an
//! invalid document never reaches the caller.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::{Map, Value, json};

use super::engine::Engine;
use super::{ErrorClass, Provider, ProviderError, endpoint_urls};

/// How the schema reaches the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuredMode {
    /// OpenAI-style `response_format` with `json_schema`.
    JsonSchema,
    /// A GBNF grammar in the `grammar` field.
    Grammar,
    /// KoboldCpp's schema-to-grammar endpoint, then a `grammar` field.
    KoboldGrammar,
    /// No grammar backend: validate after generation and repair.
    Repair,
}

impl StructuredMode {
    pub fn label(self) -> &'static str {
        match self {
            StructuredMode::JsonSchema => "json_schema",
            StructuredMode::Grammar | StructuredMode::KoboldGrammar => "grammar",
            StructuredMode::Repair => "repair",
        }
    }
}

/// A schema ready to travel with a request.
#[derive(Debug, Clone, PartialEq)]
pub struct StructuredRequest {
    pub name: String,
    /// Ref-resolved schema that is also used for response validation.
    pub schema: Value,
    pub mode: StructuredMode,
    /// GBNF for `Grammar`/`KoboldGrammar`, or the grammar returned by
    /// KoboldCpp.
    pub grammar: Option<String>,
}

impl StructuredRequest {
    /// True when the engine enforces the schema at decode time.
    pub fn strict(&self) -> bool {
        self.mode != StructuredMode::Repair
    }

    /// A visible notice for the repair fallback, or `None` when the engine
    /// enforces the schema.
    pub fn notice(&self) -> Option<String> {
        (self.mode == StructuredMode::Repair).then(|| {
            format!(
                "structured output: the engine has no grammar backend; replies are validated against {:?} and repaired when they do not match",
                self.name
            )
        })
    }

    /// Parse and validate one response body. `Err` names what failed.
    pub fn validate_response(&self, content: &str) -> Result<Value, String> {
        let content = strip_code_fence(content);
        let value: Value = serde_json::from_str(content)
            .map_err(|error| format!("the reply is not valid JSON ({error})"))?;
        validate_value(&self.schema, &value)?;
        Ok(value)
    }

    /// Write the dialect fields into a request body. The caller has already
    /// checked the tools conflict.
    pub fn apply(&self, body: &mut Value, engine: Engine) {
        match self.mode {
            StructuredMode::JsonSchema => {
                if engine == Engine::Vllm {
                    body["structured_outputs"] = json!({"json": self.schema});
                } else {
                    body["response_format"] = json!({
                        "type": "json_schema",
                        "json_schema": {
                            "name": self.name,
                            "strict": true,
                            "schema": self.schema,
                        }
                    });
                }
            }
            StructuredMode::Grammar | StructuredMode::KoboldGrammar => {
                if let Some(grammar) = &self.grammar {
                    body["grammar"] = json!(grammar);
                }
            }
            StructuredMode::Repair => {}
        }
    }
}

/// Refuse a request that combines `tools[]` with a strict response format on an
/// engine that does not support both. Returns the next action.
pub fn tools_conflict(
    engine: Engine,
    schema: &StructuredRequest,
    tools_present: bool,
) -> Option<String> {
    if !tools_present || !schema.strict() || engine.supports_tools_with_strict_output() {
        return None;
    }
    Some(format!(
        "cannot combine tools[] with a strict response format on {}: this engine does not support both. Remove the tools for this run, remove [model] response_schema, or set [model] engine to the real server if it supports both. (llama.cpp rejects a custom grammar together with tools.)",
        engine.label()
    ))
}

/// Validate the documented schema subset, name the offending keyword, resolve
/// local `$ref`s (rejecting recursion), and lower to `engine`.
pub fn plan(engine: Engine, name: &str, schema: &Value) -> Result<StructuredRequest, SchemaError> {
    let resolved = validate_and_resolve(schema)?;
    let mode = match engine.structured_output() {
        super::engine::StructuredOutput::JsonSchema => StructuredMode::JsonSchema,
        super::engine::StructuredOutput::Grammar => StructuredMode::Grammar,
        super::engine::StructuredOutput::KoboldGrammar => StructuredMode::KoboldGrammar,
        super::engine::StructuredOutput::None | super::engine::StructuredOutput::Unknown => {
            StructuredMode::Repair
        }
    };
    let grammar = match mode {
        StructuredMode::Grammar | StructuredMode::KoboldGrammar => Some(lower_gbnf(&resolved)?),
        StructuredMode::JsonSchema | StructuredMode::Repair => None,
    };
    Ok(StructuredRequest {
        name: name.to_string(),
        schema: resolved,
        mode,
        grammar,
    })
}

/// Fetch a GBNF grammar from KoboldCpp's `/api/extra/json_to_grammar`.
pub async fn kobold_grammar(
    provider: &dyn Provider,
    schema: &Value,
) -> Result<String, ProviderError> {
    let client = provider.client().ok_or_else(|| {
        ProviderError::new(
            ErrorClass::Connection,
            "the selected provider does not expose an HTTP client",
        )
    })?;
    let urls = endpoint_urls(provider.endpoint());
    let url = format!("{}/api/extra/json_to_grammar", urls.root);
    let mut request = client
        .post(&url)
        .json(&json!({"schema": schema}))
        .timeout(std::time::Duration::from_secs(10));
    if !provider.api_key().is_empty() {
        request = request.bearer_auth(provider.api_key());
    }
    let response = request.send().await.map_err(|error| {
        ProviderError::new(
            ErrorClass::Connection,
            format!("koboldcpp json_to_grammar failed: {error}"),
        )
    })?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(ProviderError::with_status(
            ErrorClass::BadRequest,
            status.as_u16(),
            format!(
                "koboldcpp json_to_grammar returned {status}: {}",
                text.chars().take(300).collect::<String>()
            ),
        ));
    }
    let value: Value = serde_json::from_str(&text).map_err(|error| {
        ProviderError::new(
            ErrorClass::Parse,
            format!("koboldcpp json_to_grammar is not JSON: {error}"),
        )
    })?;
    if let Some(grammar) = value.as_str() {
        return Ok(grammar.to_string());
    }
    for key in ["grammar", "result", "gbnf"] {
        if let Some(grammar) = value[key].as_str() {
            return Ok(grammar.to_string());
        }
    }
    Err(ProviderError::new(
        ErrorClass::Parse,
        "koboldcpp json_to_grammar returned no grammar",
    ))
}

/// A bad response schema. `keyword` is the exact key the user must remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaError {
    pub keyword: String,
    pub path: String,
    pub message: String,
}

impl SchemaError {
    fn new(keyword: impl Into<String>, path: &str, message: impl Into<String>) -> Self {
        Self {
            keyword: keyword.into(),
            path: path.to_string(),
            message: message.into(),
        }
    }
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "response schema uses unsupported keyword {:?} at {}: {}",
            self.keyword, self.path, self.message
        )
    }
}

impl std::error::Error for SchemaError {}

/// Keywords the adapter understands and lowers. Anything else is rejected
/// before a request is sent.
const SUPPORTED: &[&str] = &[
    "$schema",
    "$id",
    "$comment",
    "$defs",
    "definitions",
    "$ref",
    "title",
    "description",
    "default",
    "examples",
    "deprecated",
    "readOnly",
    "writeOnly",
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "minItems",
    "maxItems",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "format",
];

fn validate_and_resolve(schema: &Value) -> Result<Value, SchemaError> {
    validate_schema(schema, "#")?;
    let mut resolver = RefResolver::new(schema);
    resolver.resolve(schema, "#")
}

fn validate_schema(schema: &Value, path: &str) -> Result<(), SchemaError> {
    match schema {
        Value::Bool(_) => Ok(()),
        Value::Object(object) => {
            for key in object.keys() {
                if !SUPPORTED.contains(&key.as_str()) {
                    let message = if key == "oneOf" || key == "anyOf" || key == "allOf" {
                        "only the intersection represented by properties/required/items is supported"
                    } else if key == "patternProperties" {
                        "map-like objects are not supported; declare named properties"
                    } else if key == "$ref" {
                        "local refs to $defs/definitions are supported, not remote refs"
                    } else {
                        "the keyword is outside the supported JSON Schema subset"
                    };
                    return Err(SchemaError::new(key, path, message));
                }
            }
            if let Some(kind) = object.get("type")
                && !kind.is_string()
                && kind
                    .as_array()
                    .is_none_or(|types| types.len() != 1 || !types[0].is_string())
            {
                return Err(SchemaError::new(
                    "type",
                    path,
                    "type must be one string from object, array, string, number, integer, boolean, null",
                ));
            }
            if let Some(properties) = object.get("properties") {
                let properties = properties.as_object().ok_or_else(|| {
                    SchemaError::new("properties", path, "properties must be an object")
                })?;
                for (name, child) in properties {
                    validate_schema(child, &format!("{path}/properties/{name}"))?;
                }
            }
            if let Some(items) = object.get("items") {
                validate_schema(items, &format!("{path}/items"))?;
            }
            if let Some(additional) = object.get("additionalProperties")
                && !additional.is_boolean()
            {
                validate_schema(additional, &format!("{path}/additionalProperties"))?;
            }
            if let Some(defs) = object.get("$defs").or_else(|| object.get("definitions")) {
                let defs = defs
                    .as_object()
                    .ok_or_else(|| SchemaError::new("$defs", path, "$defs must be an object"))?;
                for (name, child) in defs {
                    validate_schema(child, &format!("{path}/$defs/{name}"))?;
                }
            }
            if let Some(required) = object.get("required") {
                let required = required.as_array().ok_or_else(|| {
                    SchemaError::new("required", path, "required must be an array of names")
                })?;
                if required.iter().any(|name| !name.is_string()) {
                    return Err(SchemaError::new(
                        "required",
                        path,
                        "required entries must be strings",
                    ));
                }
            }
            if let Some(values) = object.get("enum") {
                let values = values.as_array().ok_or_else(|| {
                    SchemaError::new("enum", path, "enum must be a non-empty array")
                })?;
                if values.is_empty()
                    || values.iter().any(|value| {
                        !(value.is_string()
                            || value.is_number()
                            || value.is_boolean()
                            || value.is_null())
                    })
                {
                    return Err(SchemaError::new(
                        "enum",
                        path,
                        "enum values must be strings, numbers, booleans, or null",
                    ));
                }
            }
            Ok(())
        }
        _ => Err(SchemaError::new(
            "schema",
            path,
            "a schema must be an object or a boolean",
        )),
    }
}

struct RefResolver {
    defs: BTreeMap<String, Value>,
    stack: Vec<String>,
}

impl RefResolver {
    fn new(root: &Value) -> Self {
        let mut defs = BTreeMap::new();
        for key in ["$defs", "definitions"] {
            if let Some(table) = root.get(key).and_then(Value::as_object) {
                for (name, schema) in table {
                    defs.insert(name.clone(), schema.clone());
                }
            }
        }
        Self {
            defs,
            stack: Vec::new(),
        }
    }

    fn resolve(&mut self, schema: &Value, path: &str) -> Result<Value, SchemaError> {
        match schema {
            Value::Bool(_) => Ok(schema.clone()),
            Value::Array(items) => {
                let items = items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| self.resolve(item, &format!("{path}/{index}")))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Value::Array(items))
            }
            Value::Object(object) => {
                if let Some(reference) = object.get("$ref") {
                    let reference = reference
                        .as_str()
                        .ok_or_else(|| SchemaError::new("$ref", path, "$ref must be a string"))?;
                    let name = reference
                        .strip_prefix("#/$defs/")
                        .or_else(|| reference.strip_prefix("#/definitions/"))
                        .ok_or_else(|| {
                            SchemaError::new(
                                "$ref",
                                path,
                                format!("{reference:?} is remote or not under $defs/definitions"),
                            )
                        })?;
                    let name = name.replace("~1", "/").replace("~0", "~");
                    if self.stack.iter().any(|seen| seen == &name) {
                        return Err(SchemaError::new(
                            "$ref",
                            path,
                            format!("recursive reference to {name:?} is not supported"),
                        ));
                    }
                    let target = self.defs.get(&name).cloned().ok_or_else(|| {
                        SchemaError::new(
                            "$ref",
                            path,
                            format!("reference to missing definition {name:?}"),
                        )
                    })?;
                    self.stack.push(name);
                    let mut resolved = self.resolve(&target, path)?;
                    self.stack.pop();
                    if resolved.is_object() {
                        for (key, value) in object {
                            if key != "$ref" {
                                resolved[key] = value.clone();
                            }
                        }
                    }
                    return Ok(resolved);
                }
                let mut resolved = Map::new();
                for (key, value) in object {
                    if key == "$defs" || key == "definitions" {
                        continue;
                    }
                    let child = match key.as_str() {
                        "properties" => {
                            let table = value.as_object().ok_or_else(|| {
                                SchemaError::new("properties", path, "properties must be an object")
                            })?;
                            let mut properties = Map::new();
                            for (name, child) in table {
                                properties.insert(
                                    name.clone(),
                                    self.resolve(child, &format!("{path}/properties/{name}"))?,
                                );
                            }
                            Value::Object(properties)
                        }
                        "items" => self.resolve(value, &format!("{path}/items"))?,
                        "additionalProperties" if !value.is_boolean() => {
                            self.resolve(value, &format!("{path}/additionalProperties"))?
                        }
                        _ => value.clone(),
                    };
                    resolved.insert(key.clone(), child);
                }
                Ok(Value::Object(resolved))
            }
            _ => Ok(schema.clone()),
        }
    }
}

fn strip_code_fence(content: &str) -> &str {
    let trimmed = content.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        let rest = rest
            .strip_prefix("json")
            .or_else(|| rest.strip_prefix("JSON"))
            .unwrap_or(rest);
        if let Some(inner) = rest.strip_suffix("```") {
            return inner.trim();
        }
    }
    trimmed
}

/// Validate a parsed JSON value against the same subset. This is the last
/// gate: a reply that does not match is repaired, never returned as final.
pub fn validate_value(schema: &Value, value: &Value) -> Result<(), String> {
    validate_at(schema, value, "$")
}

fn validate_at(schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    if let Value::Bool(allowed) = schema {
        return if *allowed {
            Ok(())
        } else {
            Err(format!("{path}: no value is allowed by false"))
        };
    }
    match schema {
        Value::Object(object) => {
            if let Some(constant) = object.get("const")
                && constant != value
            {
                return Err(format!("{path}: expected const {constant}, got {value}"));
            }
            if let Some(values) = object.get("enum").and_then(Value::as_array)
                && !values.contains(value)
            {
                return Err(format!("{path}: {value} is not one of the enum values"));
            }
            let kind = object
                .get("type")
                .and_then(Value::as_str)
                .or_else(|| {
                    object
                        .get("type")
                        .and_then(Value::as_array)
                        .and_then(|types| types.first())
                        .and_then(Value::as_str)
                })
                .or_else(|| {
                    (object.contains_key("properties") || object.contains_key("required"))
                        .then_some("object")
                });
            if let Some(kind) = kind
                && !matches_kind(kind, value)
            {
                return Err(format!("{path}: expected {kind}, got {}", kind_of(value)));
            }
            match value {
                Value::Object(map) => {
                    if let Some(required) = object.get("required").and_then(Value::as_array) {
                        for name in required.iter().filter_map(Value::as_str) {
                            if !map.contains_key(name) {
                                return Err(format!(
                                    "{path}: required property {name:?} is missing"
                                ));
                            }
                        }
                    }
                    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
                        for (name, child) in properties {
                            if let Some(value) = map.get(name) {
                                validate_at(child, value, &format!("{path}.{name}"))?;
                            }
                        }
                    }
                    if let Some(Value::Bool(false)) = object.get("additionalProperties") {
                        if let Some(properties) =
                            object.get("properties").and_then(Value::as_object)
                        {
                            for key in map.keys() {
                                if !properties.contains_key(key) {
                                    return Err(format!(
                                        "{path}: property {key:?} is not declared"
                                    ));
                                }
                            }
                        }
                    } else if let Some(additional) = object.get("additionalProperties")
                        && additional.is_object()
                    {
                        let properties = object.get("properties").and_then(Value::as_object);
                        for (key, child) in map {
                            if !properties.is_some_and(|properties| properties.contains_key(key)) {
                                validate_at(additional, child, &format!("{path}.{key}"))?;
                            }
                        }
                    }
                }
                Value::Array(items) => {
                    if let Some(items_schema) = object.get("items") {
                        for (index, item) in items.iter().enumerate() {
                            validate_at(items_schema, item, &format!("{path}[{index}]"))?;
                        }
                    }
                    if let Some(minimum) = object.get("minItems").and_then(Value::as_u64)
                        && items.len() < minimum as usize
                    {
                        return Err(format!("{path}: expected at least {minimum} items"));
                    }
                    if let Some(maximum) = object.get("maxItems").and_then(Value::as_u64)
                        && items.len() > maximum as usize
                    {
                        return Err(format!("{path}: expected at most {maximum} items"));
                    }
                }
                Value::String(text) => {
                    let length = text.chars().count() as u64;
                    if let Some(minimum) = object.get("minLength").and_then(Value::as_u64)
                        && length < minimum
                    {
                        return Err(format!("{path}: expected at least {minimum} characters"));
                    }
                    if let Some(maximum) = object.get("maxLength").and_then(Value::as_u64)
                        && length > maximum
                    {
                        return Err(format!("{path}: expected at most {maximum} characters"));
                    }
                }
                Value::Number(number) => {
                    let number = number.as_f64().unwrap_or(0.0);
                    if let Some(minimum) = object
                        .get("minimum")
                        .and_then(Value::as_f64)
                        .or_else(|| object.get("exclusiveMinimum").and_then(Value::as_f64))
                        && number < minimum
                    {
                        return Err(format!("{path}: {number} is below the minimum {minimum}"));
                    }
                    if let Some(maximum) = object
                        .get("maximum")
                        .and_then(Value::as_f64)
                        .or_else(|| object.get("exclusiveMaximum").and_then(Value::as_f64))
                        && number > maximum
                    {
                        return Err(format!("{path}: {number} is above the maximum {maximum}"));
                    }
                }
                _ => {}
            }
            Ok(())
        }
        _ => Err(format!("{path}: schema node is not an object")),
    }
}

fn matches_kind(kind: &str, value: &Value) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Subset the grammar backend can express. Validation still enforces required
/// fields and bounds; the grammar constrains JSON shape and property names.
fn lower_gbnf(schema: &Value) -> Result<String, SchemaError> {
    let mut lower = Gbnf::default();
    let root = lower.schema(schema, "#")?;
    let mut out = format!("root ::= {root}\n");
    for (name, rule) in &lower.rules {
        out.push_str(&format!("{name} ::= {rule}\n"));
    }
    out.push_str(GBNF_BASE);
    Ok(out)
}

/// JSON primitives every generated rule can reference. Validation still
/// enforces required properties and numeric/length bounds; the grammar makes
/// the output syntactically JSON and constrains declared property names.
const GBNF_BASE: &str = concat!(
    "ws ::= [ \\t\\n\\r]*\n",
    "json_string ::= \"\\\"\" ([^\"\\\\] | \"\\\\\" (\"\\\"\" | \"\\\\\" | \"/\" | \"b\" | \"f\" | \"n\" | \"r\" | \"t\" | \"u\" [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F]))* \"\\\"\"\n",
    "json_integer ::= \"-\"? (\"0\" | [1-9] [0-9]*)\n",
    "json_number ::= \"-\"? (\"0\" | [1-9] [0-9]*) (\".\" [0-9]+)? ([eE] [+-]? [0-9]+)?\n",
    "json_object ::= \"{\" ws (json_member (ws \",\" ws json_member)*)? ws \"}\"\n",
    "json_member ::= json_string ws \":\" ws json\n",
    "json_array ::= \"[\" ws (json (ws \",\" ws json)*)? ws \"]\"\n",
    "json ::= json_object | json_array | json_string | json_number | \"true\" | \"false\" | \"null\"\n",
);

#[derive(Default)]
struct Gbnf {
    rules: Vec<(String, String)>,
    next: usize,
}

impl Gbnf {
    fn name(&self, prefix: &str) -> String {
        format!("{prefix}_{}", self.next)
    }

    fn schema(&mut self, schema: &Value, path: &str) -> Result<String, SchemaError> {
        let object = match schema {
            Value::Bool(true) => return Ok("json".into()),
            Value::Bool(false) => {
                return Err(SchemaError::new(
                    "schema",
                    path,
                    "false schema has no grammar",
                ));
            }
            Value::Object(object) => object,
            _ => {
                return Err(SchemaError::new(
                    "schema",
                    path,
                    "a schema must be an object or a boolean",
                ));
            }
        };
        if let Some(constant) = object.get("const") {
            return Ok(literal_rule(constant));
        }
        if let Some(values) = object.get("enum").and_then(Value::as_array) {
            return Ok(values
                .iter()
                .map(literal_rule)
                .collect::<Vec<_>>()
                .join(" | "));
        }
        let kind = object.get("type").and_then(Value::as_str).or_else(|| {
            (object.contains_key("properties") || object.contains_key("required"))
                .then_some("object")
        });
        match kind {
            Some("object") => self.object(object, path),
            Some("array") => self.array(object, path),
            Some("string") => Ok("json_string".into()),
            Some("integer") => Ok("json_integer".into()),
            Some("number") => Ok("json_number".into()),
            Some("boolean") => Ok("\"true\" | \"false\"".into()),
            Some("null") => Ok("\"null\"".into()),
            Some(other) => Err(SchemaError::new(
                "type",
                path,
                format!("unknown type {other:?}"),
            )),
            None => Ok("json".into()),
        }
    }

    fn object(&mut self, object: &Map<String, Value>, path: &str) -> Result<String, SchemaError> {
        let Some(properties) = object.get("properties").and_then(Value::as_object) else {
            return Ok("json_object".into());
        };
        let mut members = Vec::new();
        for (name, child) in properties {
            let value_rule = self.schema(child, &format!("{path}/properties/{name}"))?;
            let key = gbnf_literal(name);
            members.push(format!("{key} ws \":\" ws {value_rule}"));
        }
        if members.is_empty() {
            return Ok("\"{\" ws \"}\"".into());
        }
        let name = self.name("object");
        let member_name = format!("{name}_member");
        self.rules.push((member_name.clone(), members.join(" | ")));
        self.rules.push((
            name.clone(),
            format!("\"{{\" ws ({member_name} (ws \",\" ws {member_name})*)? ws \"}}\""),
        ));
        Ok(name)
    }

    fn array(&mut self, object: &Map<String, Value>, path: &str) -> Result<String, SchemaError> {
        let item = match object.get("items") {
            Some(schema) => self.schema(schema, &format!("{path}/items"))?,
            None => "json".into(),
        };
        Ok(format!("\"[\" ws ({item} (ws \",\" ws {item})*)? ws \"]\""))
    }
}

fn literal_rule(value: &Value) -> String {
    match value {
        Value::String(text) => gbnf_literal(text),
        other => format!("\"{other}\""),
    }
}

fn gbnf_literal(value: &str) -> String {
    let mut escaped = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                escaped.push_str(&format!("\\x{:02x}", character as u32));
            }
            character => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Engine;

    fn schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["name", "count"],
            "properties": {
                "name": {"type": "string", "minLength": 1},
                "count": {"type": "integer", "minimum": 0},
                "tags": {"type": "array", "items": {"type": "string"}}
            }
        })
    }

    #[test]
    fn unsupported_keywords_are_rejected_by_name() {
        let err = plan(
            Engine::LlamaCpp,
            "x",
            &json!({"oneOf": [{"type": "string"}, {"type": "number"}]}),
        )
        .unwrap_err();
        assert_eq!(err.keyword, "oneOf");
        assert!(err.to_string().contains("oneOf"), "{err}");

        let err = plan(
            Engine::Vllm,
            "x",
            &json!({"type": "object", "patternProperties": {"^x": {"type": "string"}}}),
        )
        .unwrap_err();
        assert_eq!(err.keyword, "patternProperties");

        let err = plan(
            Engine::Sglang,
            "x",
            &json!({
                "$defs": {"node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/node"}}}},
                "$ref": "#/$defs/node"
            }),
        )
        .unwrap_err();
        assert_eq!(err.keyword, "$ref");
        assert!(err.message.contains("recursive"), "{err}");
        assert!(err.path.contains("#"));
    }

    #[test]
    fn local_refs_resolve_without_recursion() {
        let request = plan(
            Engine::Vllm,
            "x",
            &json!({
                "$defs": {"name": {"type": "string"}},
                "type": "object",
                "properties": {"name": {"$ref": "#/$defs/name"}},
                "required": ["name"]
            }),
        )
        .unwrap();
        assert_eq!(request.schema["properties"]["name"]["type"], "string");
        assert!(request.schema.get("$defs").is_none());
    }

    #[test]
    fn every_engine_gets_its_own_dialect() {
        let llama = plan(Engine::LlamaCpp, "reply", &schema()).unwrap();
        assert_eq!(llama.mode, StructuredMode::Grammar);
        assert!(llama.grammar.as_deref().unwrap().contains("root ::="));
        assert!(llama.grammar.as_deref().unwrap().contains("\"name\""));

        let vllm = plan(Engine::Vllm, "reply", &schema()).unwrap();
        assert_eq!(vllm.mode, StructuredMode::JsonSchema);
        let mut body = json!({});
        vllm.apply(&mut body, Engine::Vllm);
        assert_eq!(body["structured_outputs"]["json"]["type"], "object");
        assert!(body.get("response_format").is_none());

        let sglang = plan(Engine::Sglang, "reply", &schema()).unwrap();
        let mut body = json!({});
        sglang.apply(&mut body, Engine::Sglang);
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["name"], "reply");

        let kobold = plan(Engine::KoboldCpp, "reply", &schema()).unwrap();
        assert_eq!(kobold.mode, StructuredMode::KoboldGrammar);
        let mut body = json!({});
        kobold.apply(&mut body, Engine::KoboldCpp);
        assert!(body["grammar"].as_str().unwrap().contains("root ::="));

        let mlx = plan(Engine::Mlx, "reply", &schema()).unwrap();
        assert_eq!(mlx.mode, StructuredMode::Repair);
        assert!(mlx.notice().unwrap().contains("repair"));
        let mut body = json!({});
        mlx.apply(&mut body, Engine::Mlx);
        assert!(body.get("response_format").is_none());
        assert!(body.get("grammar").is_none());
        assert!(body.get("structured_outputs").is_none());
    }

    #[test]
    fn tools_and_a_strict_format_are_refused_on_llama_cpp() {
        let request = plan(Engine::LlamaCpp, "reply", &schema()).unwrap();
        let refusal = tools_conflict(Engine::LlamaCpp, &request, true).expect("refusal");
        assert!(refusal.contains("tools[]"), "{refusal}");
        assert!(refusal.contains("llama.cpp"), "{refusal}");
        assert!(refusal.contains("Remove the tools"), "{refusal}");
        assert!(tools_conflict(Engine::LlamaCpp, &request, false).is_none());
        let vllm = plan(Engine::Vllm, "reply", &schema()).unwrap();
        assert!(tools_conflict(Engine::Vllm, &vllm, true).is_none());
        let mlx = plan(Engine::Mlx, "reply", &schema()).unwrap();
        assert!(tools_conflict(Engine::Mlx, &mlx, true).is_none());
    }

    #[test]
    fn responses_are_validated_after_parsing() {
        let request = plan(Engine::Vllm, "reply", &schema()).unwrap();
        let value = request
            .validate_response("```json\n{\"name\":\"a\",\"count\":2}\n```")
            .unwrap();
        assert_eq!(value["count"], 2);

        let err = request.validate_response("not json").unwrap_err();
        assert!(err.contains("not valid JSON"), "{err}");

        let err = request
            .validate_response(r#"{"name":"a","count":2,"extra":1}"#)
            .unwrap_err();
        assert!(err.contains("not declared"), "{err}");

        let err = request
            .validate_response(r#"{"name":"","count":2}"#)
            .unwrap_err();
        assert!(err.contains("at least 1 characters"), "{err}");

        let err = request
            .validate_response(r#"{"name":"a","count":2.5}"#)
            .unwrap_err();
        assert!(err.contains("expected integer"), "{err}");
    }
}
