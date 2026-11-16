//! `read` with `symbol`: resolve a declaration's full extent by name.
//!
//! The range comes from a language server when one is already running for
//! the file, and otherwise from the `repo_map` scanner plus brace or
//! indentation matching for the end. The fallback says so, since it can
//! guess wrong on unusual code.

use std::path::Path;

use serde_json::{Value, json};

use crate::tools::repo_map::{Declaration, declarations};
use crate::tools::{ToolContext, ToolResult, display_path, required_str};

/// Most candidates listed for an ambiguous or unknown name.
const MAX_CANDIDATES: usize = 8;

/// Where a resolved range came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    LanguageServer,
    Scanner,
}

/// One resolved declaration: name, kind and inclusive 1-based lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extent {
    pub name: String,
    pub kind: String,
    pub start: usize,
    pub end: usize,
}

/// The outcome of resolving a name in one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Found(Extent, Source),
    Ambiguous(Vec<Extent>),
    Missing { nearest: Vec<String> },
}

/// `read {"path", "symbol"}`.
pub async fn read_symbol(ctx: &ToolContext, args: &Value) -> Result<ToolResult, ToolResult> {
    let symbol = required_str(args, "symbol")?;
    if args.get("paths").is_some()
        || ["start", "end", "limit"]
            .iter()
            .any(|key| args.get(*key).is_some())
    {
        return Err(ToolResult::err(
            "invalid_args",
            "symbol replaces start/end/limit and takes one path; pass {\"path\": \"src/a.rs\", \"symbol\": \"name\"}",
        ));
    }
    let raw = required_str(args, "path").map_err(|_| {
        ToolResult::err(
            "invalid_args",
            format!(
                "symbol needs the file it is declared in; find it with grep {{\"pattern\": \"{}\"}} and pass \"path\"",
                regex::escape(&symbol)
            ),
        )
    })?;
    let path = ctx.resolve(&raw)?;
    let display = display_path(&ctx.cwd, &path);
    // The same size limit as a plain read, checked before the whole file is
    // loaded and scanned.
    if let Ok(meta) = std::fs::metadata(&path)
        && meta.len() > crate::tools::read::MAX_BYTES
    {
        return Err(ToolResult::err(
            "FILE_TOO_LARGE",
            format!(
                "{display} is {} bytes; read caps at 4 MB. Use grep to find the declaration's line and read a range.",
                meta.len()
            ),
        ));
    }
    let text = std::fs::read(&path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|err| {
            ToolResult::err(
                "READ_FAILED",
                format!("cannot read {display}: {err}. Check the path with glob."),
            )
        })?;

    let lsp = ctx.lsp.running_symbols(&path, &text, &ctx.cwd).await;
    let resolution = match lsp {
        Some(ranges) if !ranges.is_empty() => resolve_lsp(&ranges, &symbol, &path, &text),
        _ => resolve(&path, &text, &symbol),
    };
    match resolution {
        Resolution::Found(extent, source) => {
            let range = json!({"path": raw, "start": extent.start, "end": extent.end});
            let mut result = crate::tools::read::run(ctx, &range)?;
            let shown_end = result
                .details
                .as_ref()
                .and_then(|details| details["end"].as_u64())
                .unwrap_or(extent.end as u64);
            let from = match source {
                Source::LanguageServer => "range from the language server",
                Source::Scanner => {
                    "range from the built-in scanner; the end is matched by braces or indentation"
                }
            };
            result.content = format!(
                "{} {} (lines {}-{}; {from})\n{}",
                extent.kind, extent.name, extent.start, extent.end, result.content
            );
            if let Some(details) = result.details.as_mut().and_then(Value::as_object_mut) {
                details.insert("symbol".into(), json!(extent.name));
                details.insert("symbol_end".into(), json!(extent.end));
                details.insert(
                    "symbol_source".into(),
                    json!(match source {
                        Source::LanguageServer => "lsp",
                        Source::Scanner => "scanner",
                    }),
                );
                details.insert("end".into(), json!(shown_end));
            }
            Ok(result)
        }
        Resolution::Ambiguous(candidates) => {
            let mut text = format!(
                "{symbol:?} names {} declarations in {display}; read one by its line range:",
                candidates.len()
            );
            for candidate in candidates.iter().take(MAX_CANDIDATES) {
                text.push_str(&format!(
                    "\n  {} {} (lines {}-{}): {{\"path\": \"{display}\", \"start\": {}, \"end\": {}}}",
                    candidate.kind,
                    candidate.name,
                    candidate.start,
                    candidate.end,
                    candidate.start,
                    candidate.end
                ));
            }
            Err(ToolResult::err("AMBIGUOUS_SYMBOL", text))
        }
        Resolution::Missing { nearest } => {
            let hint = if nearest.is_empty() {
                String::new()
            } else {
                format!(" Nearest declarations: {}.", nearest.join(", "))
            };
            Err(ToolResult::err(
                "UNKNOWN_SYMBOL",
                format!(
                    "no declaration named {symbol:?} in {display}.{hint} Search with grep {{\"pattern\": \"{}\", \"path\": \"{display}\"}}.",
                    regex::escape(&symbol)
                ),
            ))
        }
    }
}

/// Resolve against language-server ranges. A name the server does not report
/// falls back to the scanner, which knows other declaration forms.
fn resolve_lsp(
    ranges: &[crate::tools::lsp::SymbolRange],
    symbol: &str,
    path: &Path,
    text: &str,
) -> Resolution {
    let wanted = normalize(symbol);
    let matches: Vec<Extent> = ranges
        .iter()
        .filter(|range| {
            range.name == wanted.1
                && wanted.0.as_ref().is_none_or(|parent| {
                    range.qualified.as_deref() == Some(&format!("{parent}::{}", wanted.1))
                })
        })
        .map(|range| Extent {
            name: range
                .qualified
                .clone()
                .unwrap_or_else(|| range.name.clone()),
            kind: range.kind.to_string(),
            start: range.start,
            end: range.end,
        })
        .collect();
    match matches.len() {
        0 => resolve(path, text, symbol),
        1 => Resolution::Found(matches.into_iter().next().unwrap(), Source::LanguageServer),
        _ => Resolution::Ambiguous(matches),
    }
}

/// Split `Type::method`, `Class.method` or `impl Type` into an optional
/// container and the name.
fn normalize(symbol: &str) -> (Option<String>, String) {
    let symbol = symbol.trim();
    for separator in ["::", "."] {
        if let Some((parent, name)) = symbol.rsplit_once(separator)
            && !parent.is_empty()
            && !name.is_empty()
        {
            return (Some(parent.to_string()), name.to_string());
        }
    }
    (None, symbol.to_string())
}

/// Resolve `symbol` in `text` with the built-in scanner.
pub fn resolve(path: &Path, text: &str, symbol: &str) -> Resolution {
    let lines: Vec<&str> = text.lines().collect();
    let all = declarations(path, text);
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let extent_of = |declaration: &Declaration| Extent {
        name: declaration.name.clone(),
        kind: declaration.kind.to_string(),
        start: leading_start(&lines, declaration.line, &extension),
        end: declaration_end(&lines, declaration.line, &extension),
    };

    // `impl Type` (or any `kind name`) selects by kind as well.
    let (kind_filter, symbol) = match symbol.trim().split_once(' ') {
        Some((kind, name)) if all.iter().any(|d| d.kind == kind) => (Some(kind), name.trim()),
        _ => (None, symbol.trim()),
    };
    let (parent, name) = normalize(symbol);
    let mut matches: Vec<&Declaration> = all
        .iter()
        .filter(|d| names_match(d, &name) && kind_filter.is_none_or(|kind| d.kind == kind))
        .collect();
    if let Some(parent) = &parent {
        // Keep only the declarations inside a container named `parent`.
        let containers: Vec<Extent> = all
            .iter()
            .filter(|d| names_match(d, parent))
            .map(extent_of)
            .collect();
        matches.retain(|d| {
            containers
                .iter()
                .any(|c| d.line > c.start && d.line <= c.end)
        });
    }
    // Rust: `struct Foo` and `impl Foo` share a name; the type is what a
    // bare name means.
    if matches.len() > 1 && kind_filter.is_none() {
        let non_impl: Vec<&Declaration> = matches
            .iter()
            .copied()
            .filter(|d| d.kind != "impl")
            .collect();
        if non_impl.len() == 1 {
            matches = non_impl;
        }
    }
    match matches.len() {
        0 => Resolution::Missing {
            nearest: nearest(&all, &name),
        },
        1 => Resolution::Found(extent_of(matches[0]), Source::Scanner),
        _ => Resolution::Ambiguous(matches.into_iter().map(extent_of).collect()),
    }
}

/// `name` without generic parameters: `Runtime<'a>` is `Runtime`.
fn base_name(name: &str) -> &str {
    let name = name.trim();
    let name = if name.starts_with('<') {
        // `impl<'a> Runtime<'a>`: skip the impl's own generics.
        let mut depth = 0usize;
        let mut cut = name.len();
        for (index, c) in name.char_indices() {
            match c {
                '<' => depth += 1,
                '>' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        cut = index + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        name[cut..].trim_start()
    } else {
        name
    };
    name.split('<').next().unwrap_or(name).trim()
}

/// Whether a declaration is the one `name` asks for. An `impl` block matches
/// its type (`impl<T> Store<T>` and `impl Display for Store` both match
/// `Store`).
fn names_match(declaration: &Declaration, name: &str) -> bool {
    if declaration.name == name {
        return true;
    }
    if declaration.kind != "impl" {
        return false;
    }
    let wanted = base_name(name);
    let target = declaration
        .name
        .rsplit_once(" for ")
        .map(|(_, target)| target)
        .unwrap_or(&declaration.name);
    base_name(target) == wanted || base_name(&declaration.name) == wanted
}

/// Declarations whose names are closest to `name`.
fn nearest(all: &[Declaration], name: &str) -> Vec<String> {
    let lower = name.to_ascii_lowercase();
    let mut scored: Vec<(usize, &Declaration)> = all
        .iter()
        .filter(|d| d.kind != "impl")
        .map(|d| {
            let candidate = d.name.to_ascii_lowercase();
            let score = if candidate.contains(&lower) || lower.contains(&candidate) {
                0
            } else {
                strsim_distance(&candidate, &lower)
            };
            (score, d)
        })
        .filter(|(score, _)| *score <= name.len().max(3) / 2 + 1)
        .collect();
    scored.sort_by_key(|(score, d)| (*score, d.line));
    let mut out: Vec<String> = Vec::new();
    for (_, d) in scored {
        let label = format!("{} (line {})", d.name, d.line);
        if !out.contains(&label) {
            out.push(label);
        }
        if out.len() >= MAX_CANDIDATES {
            break;
        }
    }
    out
}

/// Levenshtein distance over chars.
fn strsim_distance(a: &str, b: &str) -> usize {
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

/// Include the doc comments, attributes and decorators directly above a
/// declaration.
fn leading_start(lines: &[&str], line: usize, extension: &str) -> usize {
    let mut start = line;
    while start > 1 {
        let above = lines[start - 2].trim_start();
        let attached = match extension {
            "py" | "pyi" => above.starts_with('@'),
            "rs" => above.starts_with("///") || above.starts_with("#[") || above.starts_with("//!"),
            "go" => above.starts_with("//"),
            "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "java" | "kt" | "cs" | "c" | "h"
            | "cc" | "cpp" | "hpp" => {
                above.starts_with("///")
                    || above.starts_with("/**")
                    || above.starts_with("* ")
                    || above == "*"
                    || above.starts_with("*/")
                    || above.starts_with('@')
            }
            _ => false,
        };
        if !attached {
            break;
        }
        start -= 1;
    }
    start
}

/// Last line of the declaration that starts at `line`.
fn declaration_end(lines: &[&str], line: usize, extension: &str) -> usize {
    match extension {
        "py" | "pyi" => indent_end(lines, line, false),
        "rb" => indent_end(lines, line, true),
        "md" | "markdown" => heading_end(lines, line),
        "toml" => section_end(lines, line),
        _ => brace_end(lines, line).unwrap_or_else(|| indent_end(lines, line, false)),
    }
}

fn indent_of(line: &str) -> usize {
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

/// Indentation languages: the block ends before the next non-blank line at
/// or left of the declaration's indent (Ruby includes its closing `end`).
fn indent_end(lines: &[&str], line: usize, include_end: bool) -> usize {
    let base = indent_of(lines[line - 1]);
    // A Python signature can span lines; skip to the line ending in ':'.
    let mut body_from = line;
    if !include_end {
        while body_from < lines.len() && !lines[body_from - 1].trim_end().ends_with(':') {
            body_from += 1;
        }
    }
    let mut last = body_from;
    for (index, text) in lines.iter().enumerate().skip(body_from) {
        if text.trim().is_empty() {
            continue;
        }
        if indent_of(text) <= base {
            if include_end && text.trim_start().starts_with("end") {
                last = index + 1;
            }
            break;
        }
        last = index + 1;
    }
    last
}

fn heading_end(lines: &[&str], line: usize) -> usize {
    let level = lines[line - 1].chars().take_while(|c| *c == '#').count();
    for (index, text) in lines.iter().enumerate().skip(line) {
        let hashes = text.chars().take_while(|c| *c == '#').count();
        if hashes > 0 && hashes <= level && text[hashes..].starts_with(' ') {
            return index.max(line);
        }
    }
    lines.len()
}

fn section_end(lines: &[&str], line: usize) -> usize {
    for (index, text) in lines.iter().enumerate().skip(line) {
        if text.trim_start().starts_with('[') {
            let mut end = index;
            while end > line && lines[end - 1].trim().is_empty() {
                end -= 1;
            }
            return end;
        }
    }
    lines.len()
}

/// Brace languages: scan from the declaration, skipping strings and
/// comments, to the line where the first opened brace closes. A `;` at depth
/// zero before any brace ends a body-less declaration (`struct Unit;`, a
/// prototype, a type alias).
fn brace_end(lines: &[&str], line: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut opened = false;
    let mut in_block_comment = false;
    // Parenthesis depth, so a `;` inside a signature's default arguments or a
    // closure type does not end the declaration.
    let mut parens = 0usize;
    for (index, text) in lines.iter().enumerate().skip(line - 1) {
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;
        let mut in_string: Option<char> = None;
        while i < chars.len() {
            let c = chars[i];
            let next = chars.get(i + 1).copied();
            if in_block_comment {
                if c == '*' && next == Some('/') {
                    in_block_comment = false;
                    i += 1;
                }
                i += 1;
                continue;
            }
            if let Some(quote) = in_string {
                if c == '\\' {
                    i += 2;
                    continue;
                }
                if c == quote {
                    in_string = None;
                }
                i += 1;
                continue;
            }
            match c {
                '/' if next == Some('/') => break,
                '/' if next == Some('*') => {
                    in_block_comment = true;
                    i += 1;
                }
                '"' | '`' => in_string = Some(c),
                '\'' => {
                    // A char literal ('x', '\n', '{'); a Rust lifetime has no
                    // closing quote right after one char.
                    if next == Some('\\') && chars.get(i + 3) == Some(&'\'') {
                        i += 3;
                    } else if chars.get(i + 2) == Some(&'\'') {
                        i += 2;
                    }
                }
                '(' => parens += 1,
                ')' => parens = parens.saturating_sub(1),
                '{' => {
                    depth += 1;
                    opened = true;
                }
                '}' => {
                    depth = depth.saturating_sub(1);
                    if opened && depth == 0 {
                        return Some(index + 1);
                    }
                }
                ';' if !opened && parens == 0 => return Some(index + 1),
                _ => {}
            }
            i += 1;
        }
        // A declaration with no brace and no `;` for a long stretch is not
        // a brace block (or the scan is lost): give up.
        if !opened && index + 1 >= line + 20 {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::write::test_context;
    use crate::tools::{Tool, read::ReadTool};

    fn found(path: &str, text: &str, symbol: &str) -> (usize, usize) {
        match resolve(Path::new(path), text, symbol) {
            Resolution::Found(extent, Source::Scanner) => (extent.start, extent.end),
            other => panic!("{symbol}: {other:?}"),
        }
    }

    const RUST: &str = "use std::fmt;\n\n/// Runs the loop.\n#[inline]\npub async fn run_loop(input: &str) -> Result<(), String> {\n    let brace = '{';\n    let text = \"}\";\n    if input.is_empty() {\n        return Err(\"empty\".into());\n    }\n    Ok(())\n}\n\npub struct Unit;\n\npub struct Runtime<'a> {\n    name: &'a str,\n}\n\nimpl<'a> Runtime<'a> {\n    pub fn run(&self) -> usize {\n        // }\n        self.name.len()\n    }\n}\n";

    #[test]
    fn rust_functions_types_and_methods() {
        assert_eq!(found("a.rs", RUST, "run_loop"), (3, 12));
        assert_eq!(found("a.rs", RUST, "Unit"), (14, 14));
        assert_eq!(found("a.rs", RUST, "Runtime"), (16, 18));
        assert_eq!(found("a.rs", RUST, "impl Runtime"), (20, 25));
        assert_eq!(found("a.rs", RUST, "run"), (21, 24));
        assert_eq!(found("a.rs", RUST, "Runtime::run"), (21, 24));
    }

    #[test]
    fn python_typescript_and_go() {
        let python = "import os\n\n@cache\ndef load(path,\n         mode='r'):\n    if path:\n\n        return open(path)\n    return None\n\nclass Store:\n    def get(self, key):\n        return key\n\n    def put(self, key):\n        pass\n\nVALUE = 1\n";
        assert_eq!(found("a.py", python, "load"), (3, 9));
        assert_eq!(found("a.py", python, "Store"), (11, 16));
        assert_eq!(found("a.py", python, "Store.get"), (12, 13));

        let ts = "import x from 'y';\n\nexport function render(props: Props): string {\n  const s = `${props.a}}`;\n  return s;\n}\n\nexport class View {\n  draw(): void {\n    if (a) { b(); }\n  }\n}\n\nexport type Props = { a: string };\n";
        assert_eq!(found("a.ts", ts, "render"), (3, 6));
        assert_eq!(found("a.ts", ts, "View"), (8, 12));
        assert_eq!(found("a.ts", ts, "Props"), (14, 14));

        let go = "package main\n\n// Serve handles requests.\nfunc Serve(addr string) error {\n\tif addr == \"\" {\n\t\treturn nil\n\t}\n\treturn nil\n}\n\ntype Server struct {\n\tAddr string\n}\n";
        assert_eq!(found("a.go", go, "Serve"), (3, 9));
        assert_eq!(found("a.go", go, "Server"), (11, 13));
    }

    #[test]
    fn language_server_ranges_win_over_the_scanner() {
        use crate::tools::lsp::SymbolRange;
        let range = |name: &str, qualified: Option<&str>, start, end| SymbolRange {
            name: name.into(),
            qualified: qualified.map(str::to_string),
            kind: "function",
            start,
            end,
        };
        let ranges = vec![
            range("run_loop", None, 5, 13),
            range("run", Some("Runtime::run"), 21, 24),
        ];
        let path = Path::new("a.rs");
        match resolve_lsp(&ranges, "run_loop", path, RUST) {
            Resolution::Found(extent, Source::LanguageServer) => {
                assert_eq!((extent.start, extent.end), (5, 13))
            }
            other => panic!("{other:?}"),
        }
        match resolve_lsp(&ranges, "Runtime::run", path, RUST) {
            Resolution::Found(extent, Source::LanguageServer) => assert_eq!(extent.start, 21),
            other => panic!("{other:?}"),
        }
        // A name the server did not report falls back to the scanner.
        assert!(matches!(
            resolve_lsp(&ranges, "Unit", path, RUST),
            Resolution::Found(_, Source::Scanner)
        ));
    }

    #[test]
    fn ambiguous_and_unknown_names() {
        let rust =
            "mod a {\n    fn helper() {}\n}\nmod b {\n    fn helper() {}\n}\nfn handler() {}\n";
        match resolve(Path::new("a.rs"), rust, "helper") {
            Resolution::Ambiguous(candidates) => {
                assert_eq!(candidates.len(), 2);
                assert_eq!((candidates[0].start, candidates[1].start), (2, 5));
            }
            other => panic!("{other:?}"),
        }
        match resolve(Path::new("a.rs"), rust, "handle") {
            Resolution::Missing { nearest } => {
                assert!(
                    nearest.iter().any(|n| n.starts_with("handler")),
                    "{nearest:?}"
                )
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn read_returns_the_declaration_and_records_its_coverage() {
        use crate::tools::LineRange;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), RUST).unwrap();
        let ctx = test_context(dir.path());

        let result = ReadTool
            .call(&ctx, json!({"path": "a.rs", "symbol": "run_loop"}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result
                .content
                .starts_with("fn run_loop (lines 3-12; range from the built-in scanner"),
            "{}",
            result.content
        );
        assert!(result.content.contains(" 3  /// Runs the loop."));
        assert!(result.content.ends_with("12  }"), "{}", result.content);
        let path = ctx.resolve("a.rs").unwrap();
        let seen = crate::tools::write::lock_reads(&ctx)
            .coverage(&path)
            .unwrap()
            .seen
            .clone();
        assert_eq!(seen, vec![LineRange { start: 3, end: 12 }]);

        let ambiguous = ReadTool
            .call(&ctx, json!({"path": "a.rs", "symbol": "missing_fn"}))
            .await;
        assert!(
            ambiguous.content.starts_with("[UNKNOWN_SYMBOL]"),
            "{}",
            ambiguous.content
        );
        assert!(ambiguous.content.contains("grep"), "{}", ambiguous.content);
    }

    #[tokio::test]
    async fn an_oversized_file_is_refused_before_it_is_scanned() {
        let dir = tempfile::tempdir().unwrap();
        let file = std::fs::File::create(dir.path().join("big.rs")).unwrap();
        file.set_len(crate::tools::read::MAX_BYTES + 1).unwrap();
        let ctx = test_context(dir.path());
        let result = ReadTool
            .call(&ctx, json!({"path": "big.rs", "symbol": "main"}))
            .await;
        assert!(
            result.content.starts_with("[FILE_TOO_LARGE]"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn a_long_body_returns_its_first_lines_with_a_trailer() {
        let dir = tempfile::tempdir().unwrap();
        let mut body = String::from("fn long() {\n");
        for n in 0..400 {
            body.push_str(&format!("    let value_{n} = compute(input_{n});\n"));
        }
        body.push_str("}\n");
        std::fs::write(dir.path().join("a.rs"), body).unwrap();
        let ctx = test_context(dir.path());
        let result = ReadTool
            .call(&ctx, json!({"path": "a.rs", "symbol": "long"}))
            .await;
        assert!(result.content.starts_with("fn long (lines 1-402;"));
        assert!(result.content.contains("1  fn long() {"));
        assert!(
            result.content.contains("of 402; next start="),
            "{}",
            result.content
        );
    }
}
