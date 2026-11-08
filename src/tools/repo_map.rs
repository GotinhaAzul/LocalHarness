//! Deterministic repository map: cached file outlines for context compression.
//!
//! The extractor is intentionally lightweight: it recognises declaration-shaped
//! lines instead of parsing a language grammar. That keeps the map fast, stable,
//! and dependency-free while still telling a model which files and symbols
//! exist. Reference graphs belong in the language-server tool, not here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use ignore::WalkBuilder;
use serde_json::{Value, json};

use crate::tools::{
    Tool, ToolContext, ToolPack, ToolResult, display_path, object_schema, optional_str,
    optional_u64,
};

pub const DEFAULT_BUDGET: usize = 2200;
pub const MAX_BUDGET: usize = 8000;
pub const BRIEF_BUDGET: usize = 1800;
const MIN_BUDGET: usize = 600;
const MAX_FILES: usize = 5000;
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_SYMBOLS_PER_FILE: usize = 24;
const BRIEF_SYMBOLS_PER_FILE: usize = 6;

#[derive(Debug, Clone)]
struct Symbol {
    line: usize,
    kind: &'static str,
    name: String,
    indent: usize,
}

#[derive(Debug, Clone)]
struct CachedFile {
    modified: Option<SystemTime>,
    size: u64,
    symbols: Vec<Symbol>,
}

#[derive(Debug, Clone)]
struct FileOutline {
    path: PathBuf,
    language: &'static str,
    symbols: Vec<Symbol>,
}

/// A process-local cache of parsed outlines. Files are re-read only when their
/// size or modification time changes, so repeated `repo_map` calls stay cheap.
#[derive(Default)]
pub struct RepoIndex {
    cache: Mutex<HashMap<PathBuf, CachedFile>>,
}

impl RepoIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// A budget-capped map of `scope` (or the whole working tree).
    pub fn map(&self, root: &Path, scope: Option<&Path>, budget: usize) -> String {
        let budget = budget.clamp(MIN_BUDGET, MAX_BUDGET);
        let files = self.scan(root, scope);
        render_map(root, &files, budget, MAX_SYMBOLS_PER_FILE)
    }

    /// The shorter map injected into the system prompt. Empty when the tree has
    /// no source files, so the prompt does not carry a useless section.
    pub fn brief(&self, root: &Path, budget: usize) -> String {
        let files = self.scan(root, None);
        if files.is_empty() {
            return String::new();
        }
        render_map(
            root,
            &files,
            budget.clamp(MIN_BUDGET, MAX_BUDGET),
            BRIEF_SYMBOLS_PER_FILE,
        )
    }

    fn scan(&self, root: &Path, scope: Option<&Path>) -> Vec<FileOutline> {
        let base = scope.unwrap_or(root);
        let mut builder = WalkBuilder::new(base);
        builder
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .parents(true)
            .follow_links(false)
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
                        | ".venv"
                        | "venv"
                        | "__pycache__"
                        | ".next"
                        | ".cache"
                        | "vendor"
                )
            });

        let mut outlines = Vec::new();
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for entry in builder.build().flatten() {
            if outlines.len() >= MAX_FILES {
                break;
            }
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let path = entry.path();
            let Some(language) = language_for(path) else {
                continue;
            };
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let size = meta.len();
            if size == 0 || size > MAX_FILE_BYTES {
                continue;
            }
            let modified = meta.modified().ok();
            let symbols = match cache.get(path) {
                Some(cached) if cached.size == size && cached.modified == modified => {
                    cached.symbols.clone()
                }
                _ => {
                    let Ok(bytes) = std::fs::read(path) else {
                        continue;
                    };
                    if bytes.iter().take(4096).any(|byte| *byte == 0) {
                        continue;
                    }
                    let text = String::from_utf8_lossy(&bytes);
                    let symbols = extract_symbols(path, &text);
                    cache.insert(
                        path.to_path_buf(),
                        CachedFile {
                            modified,
                            size,
                            symbols: symbols.clone(),
                        },
                    );
                    symbols
                }
            };
            outlines.push(FileOutline {
                path: path.to_path_buf(),
                language,
                symbols,
            });
        }
        outlines.sort_by(|left, right| {
            language_rank(left.language)
                .cmp(&language_rank(right.language))
                .then_with(|| depth(root, &left.path).cmp(&depth(root, &right.path)))
                .then_with(|| left.path.cmp(&right.path))
        });
        outlines
    }
}

fn language_rank(language: &str) -> u8 {
    match language {
        "markdown" => 1,
        "toml" => 2,
        _ => 0,
    }
}

fn depth(root: &Path, path: &Path) -> usize {
    path.strip_prefix(root)
        .map(|relative| relative.components().count())
        .unwrap_or(usize::MAX)
}

fn render_map(root: &Path, files: &[FileOutline], budget: usize, per_file: usize) -> String {
    if files.is_empty() {
        return "repo map: no source files found. Supported: Rust, Python, TypeScript/JavaScript, Go, C/C++, Java, C#, Kotlin, Ruby, Markdown, TOML, Shell.".to_string();
    }
    let total: usize = files.iter().map(|file| file.symbols.len()).sum();
    let mut out = format!("repo map: {} files, {total} symbols\n", files.len());
    let mut used = out.chars().count();
    let reserve = 90usize.min(budget / 4);
    let content_budget = budget.saturating_sub(reserve);
    let mut omitted = 0usize;
    for (index, file) in files.iter().enumerate() {
        let display = display_path(root, &file.path);
        let header = format!("\n{display} ({})", file.language);
        let header_cost = header.chars().count() + 1;
        if used + header_cost > content_budget {
            omitted = files.len() - index;
            break;
        }
        out.push_str(&header);
        out.push('\n');
        used += header_cost;
        let mut written = 0usize;
        for symbol in file.symbols.iter().take(per_file) {
            let line = format!(
                "  {}{} {} (line {})",
                "  ".repeat(symbol.indent.min(2)),
                symbol.kind,
                symbol.name,
                symbol.line
            );
            let cost = line.chars().count() + 1;
            if used + cost > content_budget {
                break;
            }
            out.push_str(&line);
            out.push('\n');
            used += cost;
            written += 1;
        }
        let remaining = file.symbols.len().saturating_sub(written);
        if remaining > 0 {
            let note = format!("  … {remaining} more symbols");
            let cost = note.chars().count() + 1;
            if used + cost <= content_budget {
                out.push_str(&note);
                out.push('\n');
                used += cost;
            }
        }
    }
    if omitted > 0 {
        let note =
            format!("\n… {omitted} files omitted; narrow with repo_map path=<dir> or raise budget");
        if used + note.chars().count() <= budget {
            out.push_str(&note);
        }
    } else if files.len() >= MAX_FILES {
        let note = format!("\n… indexed at most {MAX_FILES} files");
        if used + note.chars().count() <= budget {
            out.push_str(&note);
        }
    }
    out
}

fn language_for(path: &Path) -> Option<&'static str> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())?
        .to_ascii_lowercase();
    Some(match extension.as_str() {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "c++",
        "java" => "java",
        "cs" => "c#",
        "kt" | "kts" => "kotlin",
        "rb" => "ruby",
        "md" | "markdown" => "markdown",
        "toml" => "toml",
        "sh" | "bash" => "shell",
        _ => return None,
    })
}

fn extract_symbols(path: &Path, text: &str) -> Vec<Symbol> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut symbols = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        if symbols.len() >= MAX_SYMBOLS_PER_FILE {
            break;
        }
        let trimmed = raw.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let indent = indent_level(raw);
        if indent > 3 {
            continue;
        }
        if extension == "md" || extension == "markdown" {
            if let Some(symbol) = markdown_symbol(trimmed, index + 1, indent) {
                symbols.push(symbol);
            }
            continue;
        }
        if is_comment(&extension, trimmed) {
            continue;
        }
        if let Some(symbol) = symbol_for_line(&extension, trimmed, index + 1, indent) {
            symbols.push(symbol);
        }
    }
    symbols
}

fn indent_level(line: &str) -> usize {
    let width: usize = line
        .chars()
        .take_while(|character| *character == ' ' || *character == '\t')
        .map(|character| if character == '\t' { 4 } else { 1 })
        .sum();
    width / 4
}

fn is_comment(extension: &str, line: &str) -> bool {
    if line.starts_with("//")
        || line.starts_with("/*")
        || line.starts_with('*')
        || line.starts_with("--")
    {
        return true;
    }
    if line.starts_with('#') {
        return extension != "rs";
    }
    line.starts_with('@') && (extension == "py" || extension == "pyi")
}

fn symbol_for_line(extension: &str, line: &str, number: usize, indent: usize) -> Option<Symbol> {
    match extension {
        "rs" => rust_symbol(line, number, indent),
        "py" | "pyi" => python_symbol(line, number, indent),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => javascript_symbol(line, number, indent),
        "go" => go_symbol(line, number, indent),
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => c_symbol(line, number, indent),
        "java" | "cs" | "kt" | "kts" => java_symbol(line, number, indent),
        "rb" => ruby_symbol(line, number, indent),
        "toml" => toml_symbol(line, number, indent),
        "sh" | "bash" => shell_symbol(line, number, indent),
        _ => None,
    }
}

fn symbol(kind: &'static str, name: String, line: usize, indent: usize) -> Symbol {
    Symbol {
        line,
        kind,
        name: truncate_chars(name.trim(), 80),
        indent,
    }
}

fn rust_symbol(line: &str, number: usize, indent: usize) -> Option<Symbol> {
    let line = strip_rust_prefixes(line);
    let (kind, rest) = if let Some(rest) = line.strip_prefix("fn ") {
        ("fn", rest)
    } else if let Some(rest) = line.strip_prefix("struct ") {
        ("struct", rest)
    } else if let Some(rest) = line.strip_prefix("enum ") {
        ("enum", rest)
    } else if let Some(rest) = line.strip_prefix("trait ") {
        ("trait", rest)
    } else if let Some(rest) = line.strip_prefix("union ") {
        ("union", rest)
    } else if let Some(rest) = line.strip_prefix("mod ") {
        ("mod", rest)
    } else if let Some(rest) = line.strip_prefix("type ") {
        ("type", rest)
    } else if let Some(rest) = line.strip_prefix("macro_rules! ") {
        ("macro", rest)
    } else if line.starts_with("impl") {
        let name = line.strip_prefix("impl").unwrap_or(line).trim();
        let name = name
            .split('{')
            .next()
            .unwrap_or(name)
            .split(" where ")
            .next()
            .unwrap_or(name);
        return Some(symbol("impl", collapse_spaces(name), number, indent));
    } else {
        return None;
    };
    Some(symbol(kind, simple_name(rest), number, indent))
}

fn strip_rust_prefixes(mut line: &str) -> &str {
    loop {
        if let Some(rest) = line.strip_prefix("pub") {
            if let Some(rest) = rest.strip_prefix('(') {
                if let Some(end) = rest.find(')') {
                    line = rest[end + 1..].trim_start();
                    continue;
                }
            } else if let Some(rest) = rest.strip_prefix(' ') {
                line = rest.trim_start();
                continue;
            }
        }
        let mut stripped = false;
        for prefix in ["async ", "unsafe ", "const ", "default ", "extern "] {
            if let Some(rest) = line.strip_prefix(prefix) {
                line = rest.trim_start();
                stripped = true;
                break;
            }
        }
        if !stripped {
            break;
        }
    }
    line
}

fn python_symbol(line: &str, number: usize, indent: usize) -> Option<Symbol> {
    let line = line.strip_prefix("async ").unwrap_or(line);
    if let Some(rest) = line.strip_prefix("def ") {
        return Some(symbol("def", simple_name(rest), number, indent));
    }
    if let Some(rest) = line.strip_prefix("class ") {
        return Some(symbol("class", simple_name(rest), number, indent));
    }
    None
}

fn javascript_symbol(line: &str, number: usize, indent: usize) -> Option<Symbol> {
    let mut line = line;
    loop {
        let mut stripped = false;
        for prefix in [
            "export default ",
            "export ",
            "declare ",
            "abstract ",
            "async ",
        ] {
            if let Some(rest) = line.strip_prefix(prefix) {
                line = rest;
                stripped = true;
                break;
            }
        }
        if !stripped {
            break;
        }
    }
    for (prefix, kind) in [
        ("function ", "function"),
        ("class ", "class"),
        ("interface ", "interface"),
        ("type ", "type"),
        ("enum ", "enum"),
        ("namespace ", "namespace"),
    ] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some(symbol(kind, simple_name(rest), number, indent));
        }
    }
    for prefix in ["const ", "let ", "var "] {
        if let Some(rest) = line.strip_prefix(prefix)
            && (rest.contains("=>") || rest.contains("function"))
        {
            return Some(symbol("const", simple_name(rest), number, indent));
        }
    }
    None
}

fn go_symbol(line: &str, number: usize, indent: usize) -> Option<Symbol> {
    if let Some(rest) = line.strip_prefix("func ") {
        let rest = if rest.starts_with('(') {
            match rest.find(')') {
                Some(end) => rest[end + 1..].trim_start(),
                None => rest,
            }
        } else {
            rest
        };
        return Some(symbol("func", simple_name(rest), number, indent));
    }
    if let Some(rest) = line.strip_prefix("type ") {
        return Some(symbol("type", simple_name(rest), number, indent));
    }
    None
}

fn c_symbol(line: &str, number: usize, indent: usize) -> Option<Symbol> {
    for (prefix, kind) in [
        ("class ", "class"),
        ("struct ", "struct"),
        ("enum ", "enum"),
        ("union ", "union"),
        ("namespace ", "namespace"),
    ] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some(symbol(kind, simple_name(rest), number, indent));
        }
    }
    if indent == 0
        && line.ends_with('{')
        && line.contains('(')
        && !line.starts_with('#')
        && !has_control_keyword(line)
    {
        let before = line.split('(').next().unwrap_or("").trim();
        let name = before.split_whitespace().last().unwrap_or("");
        if !name.is_empty() && !name.contains('=') {
            return Some(symbol("fn", name.to_string(), number, indent));
        }
    }
    None
}

fn java_symbol(line: &str, number: usize, indent: usize) -> Option<Symbol> {
    for (keyword, kind) in [
        ("class ", "class"),
        ("interface ", "interface"),
        ("enum ", "enum"),
        ("record ", "record"),
        ("struct ", "struct"),
        ("object ", "object"),
        ("fun ", "fun"),
    ] {
        if let Some(position) = line.find(keyword) {
            let rest = &line[position + keyword.len()..];
            return Some(symbol(kind, simple_name(rest), number, indent));
        }
    }
    None
}

fn ruby_symbol(line: &str, number: usize, indent: usize) -> Option<Symbol> {
    if let Some(rest) = line.strip_prefix("def ") {
        return Some(symbol("def", simple_name(rest), number, indent));
    }
    if let Some(rest) = line.strip_prefix("class ") {
        return Some(symbol("class", simple_name(rest), number, indent));
    }
    if let Some(rest) = line.strip_prefix("module ") {
        return Some(symbol("module", simple_name(rest), number, indent));
    }
    None
}

fn markdown_symbol(line: &str, number: usize, _indent: usize) -> Option<Symbol> {
    let hashes = line
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if !(1..=4).contains(&hashes) {
        return None;
    }
    if line.chars().nth(hashes) != Some(' ') {
        return None;
    }
    let name = line[hashes..].trim();
    if name.is_empty() {
        return None;
    }
    Some(symbol(
        "section",
        name.to_string(),
        number,
        hashes.saturating_sub(1),
    ))
}

fn toml_symbol(line: &str, number: usize, indent: usize) -> Option<Symbol> {
    if !line.starts_with('[') {
        return None;
    }
    let name = line.trim_matches(|character| character == '[' || character == ']');
    if name.is_empty() {
        return None;
    }
    Some(symbol("table", name.to_string(), number, indent))
}

fn shell_symbol(line: &str, number: usize, indent: usize) -> Option<Symbol> {
    if let Some(rest) = line.strip_prefix("function ") {
        return Some(symbol("fn", simple_name(rest), number, indent));
    }
    if line.ends_with('{')
        && let Some((name, _)) = line.split_once("()")
        && !name.contains(char::is_whitespace)
    {
        return Some(symbol("fn", name.to_string(), number, indent));
    }
    None
}

fn has_control_keyword(line: &str) -> bool {
    line.split(|character: char| !character.is_alphanumeric() && character != '_')
        .any(|word| {
            matches!(
                word,
                "if" | "for" | "while" | "switch" | "catch" | "return" | "sizeof" | "do" | "else"
            )
        })
}

fn simple_name(text: &str) -> String {
    let name: String = text
        .trim_start()
        .chars()
        .take_while(|character| character.is_alphanumeric() || matches!(character, '_' | '$' | '~'))
        .collect();
    if name.is_empty() {
        truncate_chars(text.trim(), 60)
    } else {
        name
    }
}

fn collapse_spaces(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// The `repo_map` tool adapter.
pub struct RepoMapTool {
    index: Arc<RepoIndex>,
}

impl RepoMapTool {
    pub fn new(index: Arc<RepoIndex>) -> Self {
        Self { index }
    }
}

#[async_trait]
impl Tool for RepoMapTool {
    fn name(&self) -> &'static str {
        "repo_map"
    }

    fn description(&self) -> &'static str {
        "Compact repository map: files, symbols, and line numbers. Narrow with a path."
    }

    fn schema(&self) -> Value {
        object_schema(
            json!({
                "path": {"type": "string", "description": "Optional file or directory to map."},
                "budget": {"type": "integer", "minimum": MIN_BUDGET, "maximum": MAX_BUDGET, "description": "Character budget."}
            }),
            &[],
        )
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Coding
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let scope = match optional_str(&args, "path") {
            Some(path) if !path.trim().is_empty() => match ctx.resolve(&path) {
                Ok(path) => Some(path),
                Err(error) => return error,
            },
            _ => None,
        };
        if let Some(path) = &scope
            && !path.exists()
        {
            return ToolResult::err(
                "invalid_args",
                format!(
                    "{} does not exist; pass an existing file or directory",
                    display_path(&ctx.cwd, path)
                ),
            );
        }
        let budget = optional_u64(&args, "budget")
            .unwrap_or(DEFAULT_BUDGET as u64)
            .clamp(MIN_BUDGET as u64, MAX_BUDGET as u64) as usize;
        let text = self.index.map(&ctx.cwd, scope.as_deref(), budget);
        ToolResult::ok(text).with_details(json!({
            "budget": budget,
            "scope": scope.as_ref().map(|path| display_path(&ctx.cwd, path)),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn extracts_rust_declarations_and_impl_methods() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "src/lib.rs",
            "pub struct Runtime {\n    value: u32,\n}\n\nimpl Runtime {\n    pub async fn run(&self) {\n    }\n}\n\nfn helper() {}\n",
        );
        let files = RepoIndex::new().scan(dir.path(), None);
        assert_eq!(files.len(), 1);
        let names: Vec<String> = files[0]
            .symbols
            .iter()
            .map(|symbol| format!("{} {}", symbol.kind, symbol.name))
            .collect();
        assert_eq!(
            names,
            ["struct Runtime", "impl Runtime", "fn run", "fn helper"]
        );
    }

    #[test]
    fn extracts_python_markdown_and_toml() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "scripts/tool.py",
            "class Runner:\n    def run(self):\n        pass\n",
        );
        write(dir.path(), "README.md", "# Project\n\n## Usage\n");
        write(dir.path(), "Cargo.toml", "[package]\nname = \"demo\"\n");
        let files = RepoIndex::new().scan(dir.path(), None);
        let flat: Vec<String> = files
            .iter()
            .flat_map(|file| file.symbols.iter().map(|symbol| symbol.name.clone()))
            .collect();
        assert!(flat.contains(&"Runner".to_string()));
        assert!(flat.contains(&"run".to_string()));
        assert!(flat.contains(&"Project".to_string()));
        assert!(flat.contains(&"package".to_string()));
    }

    #[test]
    fn map_is_budget_capped_with_a_narrowing_hint() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..30 {
            write(
                dir.path(),
                &format!("src/module_{index}.rs"),
                &format!("pub fn function_{index}() {{}}\n"),
            );
        }
        let map = RepoIndex::new().map(dir.path(), None, 600);
        assert!(map.chars().count() <= 600, "{} chars", map.chars().count());
        assert!(map.contains("files omitted; narrow with repo_map"));
    }

    #[test]
    fn cache_updates_when_a_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs", "pub fn first() {}\n");
        let index = RepoIndex::new();
        let before = index.map(dir.path(), None, 1000);
        assert!(before.contains("first"));
        write(dir.path(), "src/lib.rs", "pub fn second() {}\n");
        let after = index.map(dir.path(), None, 1000);
        assert!(after.contains("second"));
        assert!(!after.contains("first"));
    }

    #[test]
    fn scope_narrows_the_map() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/a.rs", "pub fn a() {}\n");
        write(dir.path(), "tests/b.rs", "pub fn b() {}\n");
        let map = RepoIndex::new().map(dir.path(), Some(&dir.path().join("tests")), 1000);
        assert!(map.contains("tests/b.rs"));
        assert!(!map.contains("src/a.rs"));
    }

    #[tokio::test]
    async fn tool_returns_a_scoped_map() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs", "pub fn run() {}\n");
        write(dir.path(), "tests/other.rs", "pub fn other() {}\n");
        let ctx = crate::tools::write::test_context(dir.path());
        let tool = RepoMapTool::new(Arc::new(RepoIndex::new()));
        let result = tool
            .call(&ctx, serde_json::json!({"path": "src", "budget": 900}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("src/lib.rs"));
        assert!(result.content.contains("fn run"));
        assert!(!result.content.contains("tests/other.rs"));
    }
}
