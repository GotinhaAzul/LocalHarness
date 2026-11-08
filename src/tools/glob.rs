//! `glob` tool.

use std::path::Path;

use async_trait::async_trait;
use globset::Glob;
use ignore::WalkBuilder;
use serde_json::{Value, json};

use crate::tools::{
    Tool, ToolContext, ToolPack, ToolResult, display_path, optional_str, required_str,
};

const MAX_PATHS: usize = 200;

pub struct GlobTool;

impl GlobTool {}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files by glob pattern, gitignore-aware, capped at 200 paths."
    }

    fn schema(&self) -> Value {
        crate::tools::object_schema(
            json!({
                "pattern": {"type": "string", "description": "Glob to match, e.g. \"**/*.rs\"."},
                "path": {"type": "string", "description": "Directory to search (default: working directory)."}
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

fn run(ctx: &ToolContext, args: &Value) -> Result<ToolResult, ToolResult> {
    let pattern = required_str(args, "pattern")?;
    let raw = optional_str(args, "path").unwrap_or_else(|| ".".to_string());
    let root = ctx.resolve(&raw)?;
    let root_display = rel_display(&ctx.cwd, &root);

    let compiled = Glob::new(&pattern).map_err(|err| {
        ToolResult::err(
            "bad_glob",
            format!(r#"invalid glob {pattern:?}: {err}; example: "**/*.rs" matches Rust sources."#),
        )
    })?;
    let matcher = compiled.compile_matcher();

    let metadata = std::fs::metadata(&root).map_err(|err| {
        ToolResult::err(
            "NO_SUCH_PATH",
            format!("cannot search {root_display}: {err}. Pass an existing directory."),
        )
    })?;

    let mut found: Vec<String> = Vec::new();
    let mut total = 0usize;

    if metadata.is_file() {
        let label = rel_display(&ctx.cwd, &root);
        let file_name = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if matcher.is_match(&label) || matcher.is_match(&file_name) {
            total = 1;
            found.push(label);
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
                return Err(ToolResult::err("cancelled", "glob was cancelled"));
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
            let rel_root = path.strip_prefix(&root).unwrap_or(path);
            let rel_cwd = path.strip_prefix(&ctx.cwd).unwrap_or(rel_root);
            if !matcher.is_match(rel_root) && !matcher.is_match(rel_cwd) {
                continue;
            }
            total += 1;
            found.push(rel_display(&ctx.cwd, path));
        }
    }

    found.sort();
    found.truncate(MAX_PATHS);
    let truncated = total > found.len();

    if found.is_empty() {
        let text = format!("no files match {pattern:?} under {root_display}");
        return Ok(ToolResult::ok(text).with_details(json!({
            "count": 0,
            "truncated": false,
        })));
    }

    let mut text = found.join("\n");
    if truncated {
        text.push_str(&format!(
            "\n[showing {} of {total} paths; narrow the pattern or path]",
            found.len()
        ));
    }
    Ok(ToolResult::ok(text).with_details(json!({
        "count": found.len(),
        "truncated": truncated,
    })))
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
    async fn returns_sorted_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("sub/c.txt"), "").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "").unwrap();
        let ctx = test_context(dir.path());

        let result = GlobTool.call(&ctx, json!({"pattern": "**/*.txt"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "a.txt\nb.txt\nsub/c.txt");
        assert_eq!(result.details.unwrap()["count"].as_u64(), Some(3));
    }

    #[tokio::test]
    async fn caps_at_two_hundred_paths() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..205 {
            std::fs::write(dir.path().join(format!("f{index:03}.txt")), "").unwrap();
        }
        let ctx = test_context(dir.path());

        let result = GlobTool.call(&ctx, json!({"pattern": "*.txt"})).await;
        assert!(!result.is_error, "{}", result.content);
        let listed: Vec<&str> = result.content.lines().collect();
        let paths: Vec<&str> = listed
            .iter()
            .copied()
            .filter(|line| !line.starts_with('['))
            .collect();
        assert_eq!(paths.len(), 200);
        assert!(
            result.content.contains("showing 200 of 205 paths"),
            "{}",
            result.content
        );
        assert_eq!(result.details.unwrap()["truncated"].as_bool(), Some(true));
    }

    #[tokio::test]
    async fn no_matches_is_success() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        let ctx = test_context(dir.path());

        let result = GlobTool.call(&ctx, json!({"pattern": "*.nope"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("no files match"),
            "{}",
            result.content
        );
    }
}
