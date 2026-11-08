//! `grep` tool.

use std::path::Path;

use async_trait::async_trait;
use globset::Glob;
use ignore::WalkBuilder;
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};

use crate::tools::read::split_lines;
use crate::tools::{
    Tool, ToolContext, ToolPack, ToolResult, display_path, optional_bool, optional_str,
    optional_u64, required_str,
};

const MAX_MATCHES: usize = 200;
const MAX_CONTEXT: usize = 5;
const MAX_LINE_CHARS: usize = 400;
const BINARY_SNIFF: usize = 8 * 1024;

pub struct GrepTool;

impl GrepTool {}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Regex search with optional context and glob filter; gitignore-aware."
    }

    fn schema(&self) -> Value {
        crate::tools::object_schema(
            json!({
                "pattern": {"type": "string", "description": "Rust regex to search for."},
                "path": {"type": "string", "description": "File or directory to search (default: working directory)."},
                "glob": {"type": "string", "description": "Only search files matching this glob, e.g. \"**/*.rs\"."},
                "context": {"type": "integer", "description": "Lines of context before and after each match (0-5)."},
                "max_matches": {"type": "integer", "description": "Stop after this many matches (default 200)."},
                "case_sensitive": {"type": "boolean", "description": "Match case exactly (default false)."}
            }),
            &["pattern"],
        )
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Core
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        match run(ctx, &args) {
            Ok(result) => result,
            Err(err) => err,
        }
    }
}

struct ScanOutcome {
    matches: usize,
    truncated: bool,
    had_hit: bool,
}

impl ScanOutcome {
    fn none() -> Self {
        Self {
            matches: 0,
            truncated: false,
            had_hit: false,
        }
    }
}

fn run(ctx: &ToolContext, args: &Value) -> Result<ToolResult, ToolResult> {
    let pattern = required_str(args, "pattern")?;
    let context = optional_u64(args, "context")
        .unwrap_or(0)
        .min(MAX_CONTEXT as u64) as usize;
    let max_matches = optional_u64(args, "max_matches")
        .unwrap_or(MAX_MATCHES as u64)
        .clamp(1, MAX_MATCHES as u64) as usize;
    let case_sensitive = optional_bool(args, "case_sensitive").unwrap_or(false);

    let mut regex_builder = RegexBuilder::new(&pattern);
    regex_builder.case_insensitive(!case_sensitive);
    let regex = regex_builder.build().map_err(|err| {
        ToolResult::err(
            "bad_pattern",
            format!(r#"invalid regex: {err}; example: "fn\s+\w+\(" matches a function signature."#),
        )
    })?;

    let matcher = match optional_str(args, "glob") {
        Some(glob) => {
            let compiled = Glob::new(&glob).map_err(|err| {
                ToolResult::err(
                    "bad_glob",
                    format!(
                        r#"invalid glob {glob:?}: {err}; example: "**/*.rs" matches Rust sources."#
                    ),
                )
            })?;
            Some(compiled.compile_matcher())
        }
        None => None,
    };

    let raw = optional_str(args, "path").unwrap_or_else(|| ".".to_string());
    let root = ctx.resolve(&raw)?;
    let root_display = rel_display(&ctx.cwd, &root);
    let metadata = std::fs::metadata(&root).map_err(|err| {
        ToolResult::err(
            "NO_SUCH_PATH",
            format!("cannot search {root_display}: {err}. Pass an existing file or directory."),
        )
    })?;

    let mut out: Vec<String> = Vec::new();
    let mut matches = 0usize;
    let mut files_with_matches = 0usize;
    let mut truncated = false;

    if metadata.is_file() {
        let label = rel_display(&ctx.cwd, &root);
        let outcome = scan_file(&root, &label, &regex, context, max_matches, &mut out);
        matches = outcome.matches;
        truncated = outcome.truncated;
        if outcome.had_hit {
            files_with_matches = 1;
        }
    } else {
        let mut walker = WalkBuilder::new(&root);
        walker
            .hidden(true)
            .parents(true)
            .git_ignore(true)
            .git_global(true)
            .ignore(true)
            .follow_links(false);

        for entry in walker.build() {
            if ctx.is_cancelled() {
                return Err(ToolResult::err("cancelled", "grep was cancelled"));
            }
            if matches >= max_matches {
                truncated = true;
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let is_file = entry
                .file_type()
                .map(|file_type| file_type.is_file())
                .unwrap_or(false);
            if !is_file {
                continue;
            }
            let path = entry.path();
            if let Some(matcher) = &matcher {
                let rel_root = path.strip_prefix(&root).unwrap_or(path);
                let rel_cwd = path.strip_prefix(&ctx.cwd).unwrap_or(rel_root);
                if !matcher.is_match(rel_root) && !matcher.is_match(rel_cwd) {
                    continue;
                }
            }
            let label = rel_display(&ctx.cwd, path);
            let outcome = scan_file(
                path,
                &label,
                &regex,
                context,
                max_matches - matches,
                &mut out,
            );
            matches += outcome.matches;
            if outcome.had_hit {
                files_with_matches += 1;
            }
            if outcome.truncated {
                truncated = true;
            }
        }
    }

    if matches == 0 {
        let text = format!("no matches for {pattern:?} in {root_display}");
        return Ok(ToolResult::ok(text).with_details(json!({
            "matches": 0,
            "files": 0,
            "truncated": false,
        })));
    }

    let mut text = out.join("\n");
    if truncated {
        text.push_str(&format!(
            "\n[truncated at {max_matches} matches; narrow the pattern, path, or glob]"
        ));
    }
    Ok(ToolResult::ok(text).with_details(json!({
        "matches": matches,
        "files": files_with_matches,
        "truncated": truncated,
    })))
}

fn scan_file(
    path: &Path,
    label: &str,
    regex: &Regex,
    context: usize,
    budget: usize,
    out: &mut Vec<String>,
) -> ScanOutcome {
    if budget == 0 {
        return ScanOutcome::none();
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return ScanOutcome::none(),
    };
    if bytes.is_empty() {
        return ScanOutcome::none();
    }
    if bytes[..bytes.len().min(BINARY_SNIFF)].contains(&0) {
        return ScanOutcome::none();
    }

    let text = String::from_utf8_lossy(&bytes);
    let lines = split_lines(&text);
    if lines.is_empty() {
        return ScanOutcome::none();
    }

    let mut hits: Vec<usize> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if regex.is_match(line) {
            hits.push(index);
        }
    }
    if hits.is_empty() {
        return ScanOutcome::none();
    }

    let take = budget.min(hits.len());
    let mut emitted_until: Option<usize> = None;
    for hit in hits.iter().take(take) {
        let lo = hit.saturating_sub(context);
        let hi = (*hit + context).min(lines.len() - 1);
        let previous = emitted_until;
        let start = previous.map(|prev| (prev + 1).max(lo)).unwrap_or(lo);
        if start > hi {
            continue;
        }
        if previous.map(|prev| lo > prev + 1).unwrap_or(false) {
            out.push("--".to_string());
        }
        for (offset, line) in lines[start..=hi].iter().enumerate() {
            let index = start + offset;
            let line = truncate_line(line, MAX_LINE_CHARS);
            if hits.binary_search(&index).is_ok() {
                out.push(format!("{label}:{}:{line}", index + 1));
            } else {
                out.push(format!("{label}-{}-{line}", index + 1));
            }
        }
        emitted_until = Some(hi);
    }

    ScanOutcome {
        matches: take,
        truncated: hits.len() > take,
        had_hit: true,
    }
}

fn truncate_line(line: &str, max: usize) -> String {
    if line.chars().count() <= max {
        return line.to_string();
    }
    let mut truncated: String = line.chars().take(max).collect();
    truncated.push_str("...");
    truncated
}

fn rel_display(cwd: &Path, path: &Path) -> String {
    let display = display_path(cwd, path);
    if display.is_empty() {
        ".".to_string()
    } else {
        display
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::write::test_context;
    use serde_json::json;

    #[tokio::test]
    async fn reports_matches_with_context_markers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let ctx = test_context(dir.path());

        let result = GrepTool
            .call(
                &ctx,
                json!({"pattern": "two", "path": "a.txt", "context": 1, "case_sensitive": true}),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("a.txt:2:two"), "{}", result.content);
        assert!(result.content.contains("a.txt-1-one"), "{}", result.content);
        assert!(
            result.content.contains("a.txt-3-three"),
            "{}",
            result.content
        );
        assert_eq!(result.details.unwrap()["matches"].as_u64(), Some(1));
    }

    #[tokio::test]
    async fn defaults_to_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.txt"), "TWO\n").unwrap();
        let ctx = test_context(dir.path());

        let result = GrepTool.call(&ctx, json!({"pattern": "two"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("b.txt:1:TWO"), "{}", result.content);
    }

    #[tokio::test]
    async fn invalid_regex_is_reported_with_example() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());

        let result = GrepTool.call(&ctx, json!({"pattern": "["})).await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with("[bad_pattern]"),
            "{}",
            result.content
        );
        assert!(result.content.contains("example:"), "{}", result.content);
    }

    #[tokio::test]
    async fn no_matches_is_success() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let ctx = test_context(dir.path());

        let result = GrepTool.call(&ctx, json!({"pattern": "zzz"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("no matches"), "{}", result.content);
    }

    #[tokio::test]
    async fn glob_filter_limits_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "needle\n").unwrap();
        let ctx = test_context(dir.path());

        let result = GrepTool
            .call(&ctx, json!({"pattern": "needle", "glob": "*.rs"}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("b.rs:1:needle"),
            "{}",
            result.content
        );
        assert!(!result.content.contains("a.txt"), "{}", result.content);
    }
}
