//! Recovery for imperfect model output.
//!
//! Local models served through chat templates without a reasoning or tool
//! parser put `<think>` blocks in content, write tool calls as text, and
//! sometimes fall into exact repetition loops. Everything here is pure and
//! deterministic so the loop can apply it without guessing.

use serde_json::{Map, Value, json};

use super::repair::{ToolName, resolve_tool_name};

// ---------------------------------------------------------------------------
// Reasoning tags

const OPEN_TAGS: [&str; 2] = ["<think>", "<thinking>"];
const CLOSE_TAGS: [&str; 2] = ["</think>", "</thinking>"];

/// A piece of streamed content, routed by whether it sat inside a think block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    Text(String),
    Reasoning(String),
}

/// Splits `<think>...</think>` out of streamed content. Tags may arrive split
/// across deltas, so a possible tag prefix at the end of a delta is held back
/// until the next one decides it.
#[derive(Debug, Default)]
pub struct ThinkSplitter {
    inside: bool,
    opened: bool,
    pending: String,
}

impl ThinkSplitter {
    pub fn push(&mut self, delta: &str) -> Vec<Piece> {
        let mut buf = std::mem::take(&mut self.pending);
        buf.push_str(delta);
        let mut out = Vec::new();
        loop {
            let tags: &[&str] = if self.inside { &CLOSE_TAGS } else { &OPEN_TAGS };
            if let Some((at, len)) = find_first(&buf, tags) {
                self.emit(&mut out, &buf[..at]);
                buf.drain(..at + len);
                self.inside = !self.inside;
                self.opened |= self.inside;
                continue;
            }
            let keep = partial_suffix(&buf, tags);
            let head = buf.len() - keep;
            self.emit(&mut out, &buf[..head]);
            buf.drain(..head);
            self.pending = buf;
            return out;
        }
    }

    /// Emit anything held back at the end of the stream.
    pub fn flush(&mut self) -> Vec<Piece> {
        let rest = std::mem::take(&mut self.pending);
        let mut out = Vec::new();
        self.emit(&mut out, &rest);
        out
    }

    /// Whether an opening tag was seen in content.
    pub fn opened(&self) -> bool {
        self.opened
    }

    fn emit(&self, out: &mut Vec<Piece>, text: &str) {
        if text.is_empty() {
            return;
        }
        match (out.last_mut(), self.inside) {
            (Some(Piece::Reasoning(last)), true) | (Some(Piece::Text(last)), false) => {
                last.push_str(text)
            }
            (_, true) => out.push(Piece::Reasoning(text.to_string())),
            (_, false) => out.push(Piece::Text(text.to_string())),
        }
    }
}

fn find_first(haystack: &str, tags: &[&str]) -> Option<(usize, usize)> {
    tags.iter()
        .filter_map(|tag| haystack.find(tag).map(|at| (at, tag.len())))
        .min()
}

/// Length of the longest suffix of `text` that is a proper prefix of a tag.
fn partial_suffix(text: &str, tags: &[&str]) -> usize {
    tags.iter()
        .flat_map(|tag| (1..tag.len()).map(move |k| &tag[..k]))
        .filter(|prefix| text.ends_with(prefix))
        .map(str::len)
        .max()
        .unwrap_or(0)
}

/// Templates that put `<think>` in the prompt stream only the closing tag.
/// Returns `(reasoning, content)` when content holds such a dangling close.
pub fn split_unopened_think(content: &str) -> Option<(String, String)> {
    let (at, len) = find_first(content, &CLOSE_TAGS)?;
    Some((
        content[..at].trim().to_string(),
        content[at + len..].trim_start().to_string(),
    ))
}

// ---------------------------------------------------------------------------
// Repetition

/// A loop must span at least this many bytes...
const REPEAT_MIN_SPAN: usize = 600;
/// ...and repeat its unit at least this many times.
const REPEAT_MIN_REPEATS: usize = 5;
const REPEAT_MAX_PERIOD: usize = 400;
const REPEAT_CHECK_EVERY: usize = 256;

/// Watches a growing buffer for an exactly repeating tail.
#[derive(Debug, Default)]
pub struct RepetitionGuard {
    checked: usize,
}

impl RepetitionGuard {
    /// The byte length to keep when `text` now ends in a degenerate loop.
    /// Checks at most every `REPEAT_CHECK_EVERY` bytes of growth.
    pub fn check(&mut self, text: &str) -> Option<usize> {
        if text.len() < self.checked + REPEAT_CHECK_EVERY {
            return None;
        }
        self.checked = text.len();
        degenerate_tail(text)
    }
}

/// If `text` ends with one unit repeated back to back (at least
/// `REPEAT_MIN_REPEATS` times and `REPEAT_MIN_SPAN` bytes), return the byte
/// length that keeps everything before the loop plus one copy of the unit.
/// Exact periodicity never fires on ordinary prose or code.
pub fn degenerate_tail(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let same = |run: usize, period: usize| bytes[n - 1 - run] == bytes[n - 1 - run - period];
    for period in 1..=REPEAT_MAX_PERIOD.min(n / REPEAT_MIN_REPEATS) {
        let needed = (period * REPEAT_MIN_REPEATS).max(REPEAT_MIN_SPAN);
        if needed > n {
            continue;
        }
        let mut run = 0;
        while run + period < needed && same(run, period) {
            run += 1;
        }
        if run + period < needed {
            continue;
        }
        while run + period < n && same(run, period) {
            run += 1;
        }
        // Drop whole units from the end so the kept copy ends at the same
        // phase the stream did, even when the text before the loop happens
        // to end like the unit does.
        let mut keep = n - (run / period) * period;
        while !text.is_char_boundary(keep) {
            keep -= 1;
        }
        return Some(keep);
    }
    None
}

// ---------------------------------------------------------------------------
// Tool calls written as text

/// A tool call recovered from assistant text.
#[derive(Debug, Clone, PartialEq)]
pub struct TextCall {
    pub name: String,
    pub arguments: Value,
}

/// Calls recovered from text plus the prose left once they are removed.
#[derive(Debug, Clone, PartialEq)]
pub struct TextCalls {
    pub calls: Vec<TextCall>,
    pub prose: String,
    /// Short name of the syntax that matched, for the user notice.
    pub format: &'static str,
}

/// Recover tool calls a model wrote into its content.
///
/// Explicit call syntax (`<tool_call>`, `<function=...>`, `[TOOL_CALLS]`,
/// `<|python_tag|>`) is taken at its word. Fenced blocks and bare JSON lines
/// count only when every call names an active tool and the calls end the
/// response, so a model showing example JSON does not trigger anything.
pub fn extract_text_calls(content: &str, active: &[&str]) -> Option<TextCalls> {
    let finder = Finder { content, active };
    // Strongest signal first; the first syntax that yields calls wins so a
    // block is never counted twice.
    let formats = [
        "tool_call tags",
        "function tags",
        "[TOOL_CALLS]",
        "python_tag",
        "fenced block",
        "JSON line",
    ];
    for format in formats {
        let found = match format {
            "tool_call tags" => finder.tool_call_tags(),
            "function tags" => finder.function_tags(),
            "[TOOL_CALLS]" => finder.mistral(),
            "python_tag" => finder.python_tag(),
            "fenced block" => finder.fenced(),
            _ => finder.json_lines(),
        };
        if found.calls.is_empty() {
            continue;
        }
        // A model that writes a call as text stops and waits for the result.
        // Fenced or bare JSON followed by more prose is an example being
        // explained to the user, not a call.
        let weak = matches!(format, "fenced block" | "JSON line");
        let last_end = found.spans.iter().map(|(_, end)| *end).max().unwrap_or(0);
        if weak && !content[last_end..].trim().is_empty() {
            return None;
        }
        return Some(TextCalls {
            calls: found.calls,
            prose: remove_spans(content, &found.spans),
            format,
        });
    }
    None
}

#[derive(Default)]
struct Found {
    calls: Vec<TextCall>,
    spans: Vec<(usize, usize)>,
}

impl Found {
    fn push(&mut self, span: (usize, usize), calls: Vec<TextCall>) {
        if !calls.is_empty() {
            self.spans.push(span);
            self.calls.extend(calls);
        }
    }
}

struct Finder<'a> {
    content: &'a str,
    active: &'a [&'a str],
}

impl Finder<'_> {
    fn known(&self, name: &str) -> bool {
        !matches!(
            resolve_tool_name(name, self.active),
            ToolName::Unknown { .. }
        )
    }

    /// Hermes/Qwen `<tool_call>` blocks holding JSON, Qwen3-Coder XML, or
    /// GLM `<arg_key>` pairs. An unterminated final block is accepted: the
    /// closing tag is often eaten as a stop sequence.
    fn tool_call_tags(&self) -> Found {
        let mut found = Found::default();
        for (span, inner) in tagged(self.content, "<tool_call>", "</tool_call>") {
            let calls = json_calls(inner)
                .or_else(|| xml_function(inner).map(|call| vec![call]))
                .or_else(|| glm_call(inner).map(|call| vec![call]))
                .or_else(|| name_then_json(inner).map(|call| vec![call]))
                .unwrap_or_default();
            found.push(span, calls);
        }
        found
    }

    /// Bare `<function=name>` blocks with `<parameter=...>` or a JSON body.
    fn function_tags(&self) -> Found {
        let mut found = Found::default();
        for ((start, end), inner) in tagged(self.content, "<function=", "</function>") {
            let block = format!("<function={inner}</function>");
            if let Some(call) = xml_function(&block) {
                found.push((start, end), vec![call]);
            }
        }
        found
    }

    /// Mistral `[TOOL_CALLS] [{...}]` or `[TOOL_CALLS]name[ARGS]{...}`.
    fn mistral(&self) -> Found {
        let mut found = Found::default();
        const MARKER: &str = "[TOOL_CALLS]";
        let mut from = 0;
        while let Some(offset) = self.content[from..].find(MARKER) {
            let start = from + offset;
            let body_at = start + MARKER.len();
            let body = &self.content[body_at..];
            let trimmed = body.trim_start();
            let lead = body.len() - trimmed.len();
            let (calls, used) = if trimmed.starts_with('[') || trimmed.starts_with('{') {
                first_json(trimmed)
                    .map(|(value, used)| (calls_from_value(&value), used))
                    .unwrap_or_default()
            } else if let Some(args_at) = trimmed.find("[ARGS]") {
                let name = trimmed[..args_at].trim().to_string();
                first_json(&trimmed[args_at + 6..])
                    .map(|(arguments, used)| {
                        (vec![TextCall { name, arguments }], args_at + 6 + used)
                    })
                    .unwrap_or_default()
            } else {
                (Vec::new(), 0)
            };
            let end = body_at + lead + used;
            found.push((start, end), calls);
            from = end.max(body_at);
        }
        found
    }

    /// Llama `<|python_tag|>{...}`.
    fn python_tag(&self) -> Found {
        let mut found = Found::default();
        const MARKER: &str = "<|python_tag|>";
        if let Some(start) = self.content.find(MARKER) {
            let body_at = start + MARKER.len();
            let body = &self.content[body_at..];
            let trimmed = body.trim_start();
            let lead = body.len() - trimmed.len();
            if let Some((value, used)) = first_json(trimmed) {
                found.push((start, body_at + lead + used), calls_from_value(&value));
            }
        }
        found
    }

    /// Fenced blocks: JSON call objects, or Gemma-style `tool_code` calls.
    fn fenced(&self) -> Found {
        let mut found = Found::default();
        for (span, lang, body) in fences(self.content) {
            let calls = match lang {
                "tool_code" | "python" | "py" => python_call(body).into_iter().collect(),
                "" | "json" | "tool_call" | "tool" | "function" | "jsonc" => first_json(body)
                    .filter(|(_, used)| body[*used..].trim().is_empty())
                    .map(|(value, _)| calls_from_value(&value))
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            if !calls.is_empty() && calls.iter().all(|call| self.known(&call.name)) {
                found.push(span, calls);
            }
        }
        found
    }

    /// A JSON call object or array starting a line, e.g. Llama 3 writing
    /// `{"name": "read", "parameters": {...}}` after a sentence.
    fn json_lines(&self) -> Found {
        let mut found = Found::default();
        let mut line_start = 0;
        while line_start < self.content.len() {
            let line_end = self.content[line_start..]
                .find('\n')
                .map_or(self.content.len(), |at| line_start + at);
            let line = &self.content[line_start..line_end];
            let lead = line.len() - line.trim_start().len();
            let at = line_start + lead;
            let mut next = line_end + 1;
            if line[lead..].starts_with(['{', '['])
                && let Some((value, used)) = first_json(&self.content[at..])
            {
                let calls = calls_from_value(&value);
                if !calls.is_empty() && calls.iter().all(|call| self.known(&call.name)) {
                    found.push((at, at + used), calls);
                    next = next.max(at + used);
                }
            }
            line_start = next;
        }
        found
    }
}

/// `(span, inner)` for each `open ... close` block; an unterminated last block
/// runs to the end of the text.
fn tagged<'a>(text: &'a str, open: &str, close: &str) -> Vec<((usize, usize), &'a str)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(offset) = text[from..].find(open) {
        let start = from + offset;
        let inner_at = start + open.len();
        let (inner_end, end) = match text[inner_at..].find(close) {
            Some(at) => (inner_at + at, inner_at + at + close.len()),
            None => (text.len(), text.len()),
        };
        out.push(((start, end), &text[inner_at..inner_end]));
        from = end;
    }
    out
}

/// `(span, language, body)` for each fenced code block.
fn fences(text: &str) -> Vec<((usize, usize), &str, &str)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(offset) = text[from..].find("```") {
        let start = from + offset;
        let after = start + 3;
        let Some(newline) = text[after..].find('\n') else {
            break;
        };
        let lang = text[after..after + newline].trim();
        let body_at = after + newline + 1;
        let Some(close) = text[body_at..].find("```") else {
            break;
        };
        let end = body_at + close + 3;
        out.push(((start, end), lang, &text[body_at..body_at + close]));
        from = end;
    }
    out
}

/// Parse the first JSON value in `text`, returning it and the bytes used.
/// Trailing commas, a common small-model slip, are tolerated.
fn first_json(text: &str) -> Option<(Value, usize)> {
    let mut stream = serde_json::Deserializer::from_str(text).into_iter::<Value>();
    if let Some(Ok(value)) = stream.next() {
        return Some((value, stream.byte_offset()));
    }
    let end = balanced_end(text)?;
    let cleaned = strip_trailing_commas(&text[..end]);
    serde_json::from_str(&cleaned)
        .ok()
        .map(|value| (value, end))
}

/// Byte offset just past the bracket that closes the one `text` starts with.
fn balanced_end(text: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        if in_string {
            match ch {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
    }
    None
}

fn strip_trailing_commas(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    let chars: Vec<char> = text.chars().collect();
    for (index, &ch) in chars.iter().enumerate() {
        if in_string {
            match ch {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            out.push(ch);
            continue;
        }
        if ch == '"' {
            in_string = true;
        }
        if ch == ','
            && chars[index + 1..]
                .iter()
                .find(|c| !c.is_whitespace())
                .is_some_and(|c| matches!(c, '}' | ']'))
        {
            continue;
        }
        out.push(ch);
    }
    out
}

/// Every JSON value in `inner` (some models put several objects in one
/// block), as calls. `None` when the block is not call-shaped JSON.
fn json_calls(inner: &str) -> Option<Vec<TextCall>> {
    let mut rest = inner.trim();
    let mut calls = Vec::new();
    while !rest.is_empty() {
        let (value, used) = first_json(rest)?;
        let found = calls_from_value(&value);
        if found.is_empty() {
            return None;
        }
        calls.extend(found);
        rest = rest[used..]
            .trim_start()
            .trim_start_matches(',')
            .trim_start();
    }
    (!calls.is_empty()).then_some(calls)
}

/// One call object or an array of them, in any of the common shapes:
/// `{"name", "arguments"|"parameters"|"args"|"input"}`, OpenAI's
/// `{"function": {"name", "arguments"}}`, or a flat `{"name", ...args}`.
fn calls_from_value(value: &Value) -> Vec<TextCall> {
    match value {
        Value::Array(items) => {
            let calls: Vec<TextCall> = items.iter().filter_map(call_from_object).collect();
            if calls.len() == items.len() {
                calls
            } else {
                Vec::new()
            }
        }
        _ => call_from_object(value).into_iter().collect(),
    }
}

fn call_from_object(value: &Value) -> Option<TextCall> {
    let outer = value.as_object()?;
    let target = outer
        .get("function")
        .and_then(Value::as_object)
        .unwrap_or(outer);
    const NAME_KEYS: [&str; 4] = ["name", "tool", "tool_name", "function"];
    const ARG_KEYS: [&str; 5] = ["arguments", "parameters", "args", "input", "params"];
    let name = NAME_KEYS
        .iter()
        .find_map(|key| target.get(*key).and_then(Value::as_str))?
        .trim()
        .to_string();
    if name.is_empty() {
        return None;
    }
    let arguments = match ARG_KEYS.iter().find_map(|key| target.get(*key)) {
        Some(Value::String(raw)) => serde_json::from_str::<Value>(raw)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| Value::String(raw.clone())),
        Some(other) => other.clone(),
        None => {
            let flat: Map<String, Value> = target
                .iter()
                .filter(|(key, _)| !NAME_KEYS.contains(&key.as_str()) && *key != "type")
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            Value::Object(flat)
        }
    };
    Some(TextCall { name, arguments })
}

/// Qwen3-Coder style:
/// `<function=read><parameter=path>src/a.rs</parameter></function>`, or a
/// JSON body inside `<function=read>...</function>`.
fn xml_function(inner: &str) -> Option<TextCall> {
    let at = inner.find("<function=")?;
    let rest = &inner[at + "<function=".len()..];
    let close = rest.find('>')?;
    let name = rest[..close].trim().trim_matches(['"', '\'']).to_string();
    if name.is_empty() {
        return None;
    }
    let body = &rest[close + 1..];
    let body = body.find("</function>").map_or(body, |end| &body[..end]);
    let mut arguments = Map::new();
    for (_, param) in tagged(body, "<parameter=", "</parameter>") {
        let Some(split) = param.find('>') else {
            continue;
        };
        let key = param[..split].trim().trim_matches(['"', '\'']).to_string();
        arguments.insert(key, json!(trim_one_newline(&param[split + 1..])));
    }
    if arguments.is_empty() {
        let trimmed = body.trim();
        if trimmed.starts_with('{') {
            let (value, _) = first_json(trimmed)?;
            return Some(TextCall {
                name,
                arguments: value,
            });
        }
    }
    Some(TextCall {
        name,
        arguments: Value::Object(arguments),
    })
}

/// GLM style: `read\n<arg_key>path</arg_key>\n<arg_value>a.rs</arg_value>`.
fn glm_call(inner: &str) -> Option<TextCall> {
    let first_key = inner.find("<arg_key>")?;
    let name = inner[..first_key].trim().to_string();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    let keys = tagged(&inner[first_key..], "<arg_key>", "</arg_key>");
    let values = tagged(&inner[first_key..], "<arg_value>", "</arg_value>");
    let mut arguments = Map::new();
    for ((_, key), (_, value)) in keys.iter().zip(values.iter()) {
        let value = trim_one_newline(value);
        let parsed = serde_json::from_str::<Value>(value)
            .ok()
            .filter(|v| v.is_array() || v.is_object())
            .unwrap_or_else(|| json!(value));
        arguments.insert(key.trim().to_string(), parsed);
    }
    Some(TextCall {
        name,
        arguments: Value::Object(arguments),
    })
}

/// `name\n{json}` inside a tool_call block.
fn name_then_json(inner: &str) -> Option<TextCall> {
    let trimmed = inner.trim();
    let brace = trimmed.find('{')?;
    let name = trimmed[..brace].trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    let (arguments, _) = first_json(&trimmed[brace..])?;
    arguments.is_object().then(|| TextCall {
        name: name.to_string(),
        arguments,
    })
}

/// Gemma `tool_code`: `read(path="src/a.rs", limit=40)`, optionally wrapped
/// in `print(...)` and prefixed with a module such as `default_api.`.
fn python_call(source: &str) -> Option<TextCall> {
    let mut text = source.trim();
    if let Some(inner) = text.strip_prefix("print(") {
        text = inner.strip_suffix(')')?.trim();
    }
    let open = text.find('(')?;
    let path = text[..open].trim();
    if path.is_empty()
        || !path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    {
        return None;
    }
    let name = path.rsplit('.').next()?.to_string();
    let body = text[open + 1..].trim_end().strip_suffix(')')?;
    let mut arguments = Map::new();
    let mut rest = body.trim();
    while !rest.is_empty() {
        let eq = rest.find('=')?;
        let key = rest[..eq].trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return None;
        }
        let (value, used) = python_literal(rest[eq + 1..].trim_start())?;
        arguments.insert(key.to_string(), value);
        let consumed = rest.len() - rest[eq + 1..].trim_start().len() + used;
        rest = rest[consumed..].trim_start();
        rest = rest.strip_prefix(',').unwrap_or(rest).trim_start();
    }
    Some(TextCall {
        name,
        arguments: Value::Object(arguments),
    })
}

/// A Python string, number, bool, None, or JSON-compatible list/dict literal.
fn python_literal(text: &str) -> Option<(Value, usize)> {
    let first = text.chars().next()?;
    if first == '"' || first == '\'' {
        let mut out = String::new();
        let mut escaped = false;
        for (index, ch) in text.char_indices().skip(1) {
            if escaped {
                out.push(match ch {
                    'n' => '\n',
                    't' => '\t',
                    other => other,
                });
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == first {
                return Some((json!(out), index + 1));
            } else {
                out.push(ch);
            }
        }
        return None;
    }
    if first == '[' || first == '{' {
        return first_json(text);
    }
    let end = text.find([',', ')']).unwrap_or(text.len());
    let token = text[..end].trim();
    let value = match token {
        "True" | "true" => json!(true),
        "False" | "false" => json!(false),
        "None" | "null" => Value::Null,
        _ => serde_json::from_str::<Value>(token)
            .ok()
            .filter(Value::is_number)?,
    };
    Some((value, end))
}

fn trim_one_newline(text: &str) -> &str {
    let text = text.strip_prefix('\n').unwrap_or(text);
    text.strip_suffix('\n').unwrap_or(text)
}

fn remove_spans(text: &str, spans: &[(usize, usize)]) -> String {
    let mut spans = spans.to_vec();
    spans.sort_unstable();
    let mut out = String::new();
    let mut from = 0;
    for (start, end) in spans {
        if start >= from {
            out.push_str(&text[from..start]);
            from = end;
        }
    }
    out.push_str(&text[from.min(text.len())..]);
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTIVE: &[&str] = &["read", "edit", "bash", "grep", "write"];

    fn split_all(chunks: &[&str]) -> (String, String) {
        let mut splitter = ThinkSplitter::default();
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut pieces: Vec<Piece> = chunks.iter().flat_map(|c| splitter.push(c)).collect();
        pieces.extend(splitter.flush());
        for piece in pieces {
            match piece {
                Piece::Text(t) => text.push_str(&t),
                Piece::Reasoning(r) => reasoning.push_str(&r),
            }
        }
        (text, reasoning)
    }

    #[test]
    fn think_blocks_split_even_across_chunk_boundaries() {
        let (text, reasoning) = split_all(&["<thi", "nk>plan it", "</th", "ink>\n\nAnswer < 3"]);
        assert_eq!(reasoning, "plan it");
        assert_eq!(text, "\n\nAnswer < 3");
        let (text, reasoning) = split_all(&["no tags here <", "b>bold</b>"]);
        assert_eq!(
            (text.as_str(), reasoning.as_str()),
            ("no tags here <b>bold</b>", "")
        );
        // Ran out of tokens while thinking: it all stays reasoning.
        let (text, reasoning) = split_all(&["<thinking>still going"]);
        assert_eq!((text.as_str(), reasoning.as_str()), ("", "still going"));
    }

    #[test]
    fn dangling_close_tag_marks_prompt_side_thinking() {
        let (reasoning, content) = split_unopened_think("let me see</think>\n\nDone.").unwrap();
        assert_eq!(reasoning, "let me see");
        assert_eq!(content, "Done.");
        assert!(split_unopened_think("plain").is_none());
    }

    #[test]
    fn exact_loops_are_cut_to_one_copy() {
        let head = "Here is the plan.\n";
        let unit = "I will now read the file to check.\n";
        let text = format!("{head}{}", unit.repeat(30));
        let keep = degenerate_tail(&text).expect("loop detected");
        assert_eq!(&text[..keep], format!("{head}{unit}"));
        let dashes = format!("table\n{}", "-".repeat(700));
        assert!(degenerate_tail(&dashes).is_some());
    }

    #[test]
    fn ordinary_text_and_code_are_not_loops() {
        let code: String = (0..200)
            .map(|n| format!("    assert_eq!(parse(\"{n}\"), Ok({n}));\n"))
            .collect();
        assert!(degenerate_tail(&code).is_none());
        let prose = "The parser reads tokens and builds a tree. ".repeat(4);
        assert!(
            degenerate_tail(&prose).is_none(),
            "4 repeats is below the bar"
        );
        let mut guard = RepetitionGuard::default();
        assert!(guard.check("short").is_none());
    }

    fn one(content: &str) -> (TextCall, String, &'static str) {
        let found =
            extract_text_calls(content, ACTIVE).unwrap_or_else(|| panic!("no call in {content:?}"));
        assert_eq!(found.calls.len(), 1, "{found:?}");
        (found.calls[0].clone(), found.prose, found.format)
    }

    #[test]
    fn hermes_tool_call_tags() {
        let (call, prose, format) = one(
            "Reading it.\n<tool_call>\n{\"name\": \"read\", \"arguments\": {\"path\": \"a.rs\"}}\n</tool_call>",
        );
        assert_eq!(call.name, "read");
        assert_eq!(call.arguments, json!({"path": "a.rs"}));
        assert_eq!(prose, "Reading it.");
        assert_eq!(format, "tool_call tags");
        // Closing tag eaten by a stop sequence; trailing comma slip.
        let (call, _, _) =
            one("<tool_call>{\"name\":\"bash\",\"arguments\":{\"command\":\"ls\",}}");
        assert_eq!(call.arguments, json!({"command": "ls"}));
    }

    #[test]
    fn several_calls_in_one_response() {
        let found = extract_text_calls(
            "<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"a\"}}</tool_call>\n<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"b\"}}</tool_call>",
            ACTIVE,
        )
        .unwrap();
        assert_eq!(found.calls.len(), 2);
        assert_eq!(found.calls[1].arguments["path"], "b");
        assert_eq!(found.prose, "");
    }

    #[test]
    fn qwen3_coder_xml_calls() {
        let (call, _, _) = one(
            "<tool_call>\n<function=edit>\n<parameter=path>\nsrc/a.rs\n</parameter>\n<parameter=old>\nlet x = 1;\n</parameter>\n<parameter=new>\nlet x = 2;\n</parameter>\n</function>\n</tool_call>",
        );
        assert_eq!(call.name, "edit");
        assert_eq!(
            call.arguments,
            json!({"path": "src/a.rs", "old": "let x = 1;", "new": "let x = 2;"})
        );
        let (call, _, format) = one("<function=read>{\"path\": \"a.rs\"}</function>");
        assert_eq!(call.arguments, json!({"path": "a.rs"}));
        assert_eq!(format, "function tags");
    }

    #[test]
    fn glm_arg_pairs() {
        let (call, _, _) = one(
            "<tool_call>grep\n<arg_key>pattern</arg_key>\n<arg_value>fn main</arg_value>\n<arg_key>context</arg_key>\n<arg_value>2</arg_value>\n</tool_call>",
        );
        assert_eq!(call.name, "grep");
        assert_eq!(
            call.arguments,
            json!({"pattern": "fn main", "context": "2"})
        );
    }

    #[test]
    fn mistral_and_llama_markers() {
        let (call, _, _) =
            one("[TOOL_CALLS] [{\"name\": \"read\", \"arguments\": {\"path\": \"a\"}}]");
        assert_eq!(call.arguments["path"], "a");
        let (call, _, _) = one("[TOOL_CALLS]bash[ARGS]{\"command\": \"pwd\"}");
        assert_eq!(
            (call.name.as_str(), &call.arguments),
            ("bash", &json!({"command": "pwd"}))
        );
        let (call, _, _) =
            one("<|python_tag|>{\"name\": \"read\", \"parameters\": {\"path\": \"a\"}}");
        assert_eq!(call.arguments["path"], "a");
    }

    #[test]
    fn fenced_and_bare_json_need_a_known_tool() {
        let (call, prose, _) = one(
            "Let me look.\n```json\n{\"name\": \"read_file\", \"arguments\": {\"file_path\": \"a\"}}\n```",
        );
        assert_eq!(call.name, "read_file", "run.rs routes and notes the alias");
        assert_eq!(prose, "Let me look.");
        let (call, _, _) =
            one("I'll search.\n{\"name\": \"grep\", \"parameters\": {\"pattern\": \"x\"}}");
        assert_eq!(call.name, "grep");
        // OpenAI-shaped objects work too.
        let (call, _, _) = one(
            "{\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a\\\"}\"}}",
        );
        assert_eq!(call.arguments, json!({"path": "a"}));
        // A call-shaped example followed by more explanation is not a call.
        assert!(
            extract_text_calls(
                "You could run:\n```json\n{\"name\": \"bash\", \"arguments\": {\"command\": \"rm -rf build\"}}\n```\nThat clears the build directory.",
                ACTIVE
            )
            .is_none()
        );
        // Example JSON about something else is left alone.
        assert!(
            extract_text_calls("```json\n{\"name\": \"widget\", \"size\": 3}\n```", ACTIVE)
                .is_none()
        );
        assert!(extract_text_calls("Use {\"name\": \"x\"} in config.", ACTIVE).is_none());
        assert!(extract_text_calls("Plain answer with no calls.", ACTIVE).is_none());
    }

    #[test]
    fn gemma_tool_code() {
        let (call, _, _) =
            one("```tool_code\nprint(default_api.read(path=\"src/a.rs\", limit=40))\n```");
        assert_eq!(call.name, "read");
        assert_eq!(call.arguments, json!({"path": "src/a.rs", "limit": 40}));
        let (call, _, _) = one("```tool_code\nbash(command='echo \"hi\"', timeout=5)\n```");
        assert_eq!(
            call.arguments,
            json!({"command": "echo \"hi\"", "timeout": 5})
        );
    }
}
