//! `@path` file mentions in user prompts.
//!
//! A prompt like `fix the bug in @src/parser.rs` attaches a snapshot of that
//! file to the user message, so the model starts with the code instead of
//! spending a round trip on `read`. Each mention goes through the `read`
//! tool's own code path: the path jail, binary and size checks, line
//! numbering, and the read stamp that lets the model `edit` the file without
//! re-reading it all apply unchanged.

use serde_json::json;

use crate::session::Attachment;
use crate::tools::ToolContext;

/// Most files one prompt may attach.
pub const MAX_MENTIONS: usize = 8;

/// The result of resolving the mentions in one prompt.
#[derive(Debug, Default)]
pub struct Resolved {
    pub attachments: Vec<Attachment>,
    /// Human-readable reasons a mention was not attached.
    pub skipped: Vec<String>,
}

/// `@path` tokens in `text`, in order and without duplicates. A mention
/// starts at the beginning of the text or after whitespace (so e-mail
/// addresses do not count), and trailing sentence punctuation is dropped.
pub fn parse(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        let Some(path) = word.strip_prefix('@') else {
            continue;
        };
        let path = path
            .trim_start_matches(['"', '\'', '`'])
            .trim_end_matches(['"', '\'', '`', ',', ';', ':', '!', '?', ')', ']'])
            .trim_end_matches('.');
        if path.is_empty() || found.iter().any(|seen| seen == path) {
            continue;
        }
        found.push(path.to_string());
    }
    found
}

/// Attach every mentioned file that exists, within `char_budget` characters
/// in total. Mentions that name nothing on disk are left alone silently:
/// `@someone` in prose is not an error. Real files that cannot be attached
/// (binary, outside the working directory, over budget) are reported.
pub fn resolve(ctx: &ToolContext, text: &str, char_budget: usize) -> Resolved {
    let mut resolved = Resolved::default();
    let mut used = 0usize;
    for raw in parse(text) {
        let candidate = if std::path::Path::new(&raw).is_absolute() {
            std::path::PathBuf::from(&raw)
        } else {
            ctx.cwd.join(&raw)
        };
        if !candidate.is_file() {
            continue;
        }
        if resolved.attachments.len() >= MAX_MENTIONS {
            resolved
                .skipped
                .push(format!("@{raw}: more than {MAX_MENTIONS} files mentioned"));
            continue;
        }
        let result = match crate::tools::read::run(ctx, &json!({ "path": raw, "limit": 800 })) {
            Ok(result) | Err(result) => result,
        };
        if result.is_error {
            resolved.skipped.push(format!("@{raw}: {}", result.content));
            continue;
        }
        let size = result.content.chars().count();
        if used + size > char_budget {
            resolved.skipped.push(format!(
                "@{raw}: too large to attach ({size} chars, {} left in the attachment budget); the model can read it",
                char_budget.saturating_sub(used)
            ));
            continue;
        }
        used += size;
        let path = result
            .details
            .as_ref()
            .and_then(|details| details.get("path"))
            .and_then(|path| path.as_str())
            .unwrap_or(&raw)
            .to_string();
        resolved.attachments.push(Attachment {
            path,
            content: result.content,
        });
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::write::test_context;

    #[test]
    fn parses_mentions_but_not_emails() {
        assert_eq!(
            parse("fix @src/a.rs, then @src/b.rs. mail me@example.com @src/a.rs"),
            vec!["src/a.rs", "src/b.rs"]
        );
        assert_eq!(parse("(see @`notes.md`)"), vec!["notes.md"]);
        assert!(parse("just @ alone").is_empty());
    }

    #[test]
    fn attaches_existing_files_and_records_the_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("bin.dat"), [0u8, 1, 2]).unwrap();
        let ctx = test_context(dir.path());

        let resolved = resolve(&ctx, "look at @a.rs and @bin.dat, ping @someone", 10_000);
        assert_eq!(resolved.attachments.len(), 1);
        assert_eq!(resolved.attachments[0].path, "a.rs");
        assert!(resolved.attachments[0].content.contains("1  fn main() {}"));
        // The binary file is reported; the non-file mention is ignored.
        assert_eq!(resolved.skipped.len(), 1, "{:?}", resolved.skipped);
        assert!(resolved.skipped[0].starts_with("@bin.dat"));

        // The read stamp lets edit/write proceed without a separate read.
        let reads = crate::tools::write::lock_reads(&ctx);
        assert!(reads.is_current(&dir.path().join("a.rs")));
    }

    #[test]
    fn respects_the_character_budget() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.txt"), "x".repeat(500)).unwrap();
        let ctx = test_context(dir.path());
        let resolved = resolve(&ctx, "@big.txt", 100);
        assert!(resolved.attachments.is_empty());
        assert!(resolved.skipped[0].contains("too large"));
    }
}
