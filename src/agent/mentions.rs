//! `@path` file mentions in user prompts.
//!
//! A prompt like `fix the bug in @src/parser.rs` attaches a snapshot of that
//! file to the user message, so the model starts with the code instead of
//! spending a round trip on `read`. Each mention goes through the `read`
//! tool's own code path: the path jail, binary and size checks, line
//! numbering, and the read stamp that lets the model `edit` the file without
//! re-reading it all apply unchanged.
//!
//! A path written without `@` (`the panic in src/parser.rs:88`, a pasted
//! stack trace) is attached the same way when it names an existing text file
//! in the working directory: at most [`MAX_NAMED`] per prompt, after the
//! `@` mentions, within the same budget, and silently skipped otherwise.
//! `[prompt] attach_named_files = false` turns this off.
//!
//! An image mention (`@shot.png`, recognised by content) is validated,
//! stripped of metadata, stored as an artifact, and either sent as an image
//! (a model with vision input) or described once by `[vision] model` and
//! attached as labelled text.

use serde_json::json;

use crate::media::{Route, image};
use crate::provider::ImageData;
use crate::session::{Attachment, ImageRef};
use crate::tools::{ToolContext, display_path};

/// Most files one prompt may attach.
pub const MAX_MENTIONS: usize = 8;
/// Most files attached because the prompt named them without `@`.
pub const MAX_NAMED: usize = 3;

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

/// Path-like words in `text` written without `@`, in order and without
/// duplicates: a word with a `/` or a file extension, with quotes, brackets,
/// trailing punctuation, and a `:line[:col]` or `(line,col)` suffix removed.
/// URLs, options, and `~` paths are not candidates. Whether one names a file
/// is decided by [`resolve`].
pub fn parse_named(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        if word.starts_with('@') || word.contains("://") {
            continue;
        }
        let mut path = word
            .trim_start_matches(['"', '\'', '`', '(', '[', '<'])
            .trim_end_matches(['"', '\'', '`', ',', ';', '!', '?', ')', ']', '>', '.']);
        // `src/a.rs:12:5`, `src/a.rs:12:`, `src/a.ts(12,5):`.
        loop {
            if let Some(head) = path.strip_suffix(':') {
                path = head;
            } else if let Some((head, tail)) = path.rsplit_once(':')
                && !tail.is_empty()
                && tail.chars().all(|c| c.is_ascii_digit())
            {
                path = head;
            } else if let Some(open) = path.rfind('(')
                && path.ends_with(')')
                && path[open + 1..path.len() - 1]
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == ',')
            {
                path = &path[..open];
            } else {
                break;
            }
        }
        let path = path.trim_start_matches("./");
        if path.is_empty()
            || path.starts_with('-')
            || path.starts_with('~')
            || !path_like(path)
            || found.iter().any(|seen| seen == path)
        {
            continue;
        }
        found.push(path.to_string());
    }
    found
}

/// A word that could be a path: it has a directory separator, or a file
/// name with an extension that is not just digits (`v1.2`, `3.14`).
fn path_like(word: &str) -> bool {
    if word.contains('/') {
        return word.chars().any(|c| c.is_ascii_alphabetic());
    }
    match word.rsplit_once('.') {
        Some((stem, ext)) => {
            !stem.is_empty()
                && !ext.is_empty()
                && ext.len() <= 10
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
                && !ext.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

/// Attach every mentioned file that exists, within `char_budget` characters
/// in total. Mentions that name nothing on disk are left alone silently:
/// `@someone` in prose is not an error. Real files that cannot be attached
/// (binary, outside the working directory, over budget) are reported.
pub async fn resolve(ctx: &ToolContext, text: &str, char_budget: usize) -> Resolved {
    resolve_with(ctx, text, char_budget, false).await
}

/// [`resolve`], then, when `named` is set, files the prompt names without
/// `@` (see [`parse_named`]).
pub async fn resolve_with(
    ctx: &ToolContext,
    text: &str,
    char_budget: usize,
    named: bool,
) -> Resolved {
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
        if image::is_image_file(&candidate) {
            match attach_image(ctx, &raw).await {
                Ok((attachment, cost)) if used + cost <= char_budget => {
                    used += cost;
                    resolved.attachments.push(attachment);
                }
                Ok((_, cost)) => resolved.skipped.push(format!(
                    "@{raw}: too large to attach ({cost} chars, {} left in the attachment budget)",
                    char_budget.saturating_sub(used)
                )),
                Err(reason) => resolved.skipped.push(format!("@{raw}: {reason}")),
            }
            continue;
        }
        let result = match crate::tools::read::run_with_budget(
            ctx,
            &json!({ "path": raw, "limit": 800 }),
            usize::MAX,
        ) {
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
        resolved
            .attachments
            .push(Attachment::file(path, result.content));
    }
    if named {
        attach_named(ctx, text, char_budget, &mut used, &mut resolved);
    }
    resolved
}

/// Attach up to [`MAX_NAMED`] text files the prompt names without `@`.
/// Nothing here is reported: a word that only looks like a path, a file
/// outside the jail, a binary, or one over budget is simply not attached.
fn attach_named(
    ctx: &ToolContext,
    text: &str,
    char_budget: usize,
    used: &mut usize,
    resolved: &mut Resolved,
) {
    let mut attached = 0usize;
    for raw in parse_named(text) {
        if attached >= MAX_NAMED || resolved.attachments.len() >= MAX_MENTIONS {
            break;
        }
        let Ok(path) = ctx.resolve(&raw) else {
            continue;
        };
        // Only the project's own files: a named path is never an invitation
        // to read outside the working directory, jail or not.
        if !path.starts_with(&ctx.cwd)
            || !path.is_file()
            || image::is_image_file(&path)
            || path.components().any(|part| part.as_os_str() == ".git")
        {
            continue;
        }
        let display = display_path(&ctx.cwd, &path);
        if resolved
            .attachments
            .iter()
            .any(|attachment| attachment.path == display)
        {
            continue;
        }
        let result = match crate::tools::read::run_with_budget(
            ctx,
            &json!({ "path": display, "limit": 800 }),
            usize::MAX,
        ) {
            Ok(result) | Err(result) => result,
        };
        if result.is_error {
            continue;
        }
        let size = result.content.chars().count();
        if *used + size > char_budget {
            continue;
        }
        *used += size;
        attached += 1;
        resolved
            .attachments
            .push(Attachment::file(display, result.content));
    }
}

/// Validate, store, and route one image. Returns the attachment and its
/// character cost against the attachment budget.
async fn attach_image(ctx: &ToolContext, raw: &str) -> Result<(Attachment, usize), String> {
    let (image_ref, loaded, display) = store_image(ctx, raw)?;
    match ctx.vision.route() {
        Route::Native => {
            let cost = image_data(&loaded, &display).estimated_tokens() * 7 / 2;
            Ok((
                Attachment {
                    path: display,
                    content: String::new(),
                    image: Some(image_ref),
                    described_by: None,
                },
                cost,
            ))
        }
        Route::Describe(model) => {
            let describer = ctx
                .vision
                .describer
                .as_ref()
                .ok_or("no vision model is configured")?;
            let description = crate::media::vision::describe(
                describer,
                image_data(&loaded, &display),
                ctx.cancel.clone(),
            )
            .await?;
            let cost = description.len();
            Ok((
                Attachment {
                    path: display,
                    content: description,
                    image: Some(image_ref),
                    described_by: Some(model),
                },
                cost,
            ))
        }
        Route::Refused(reason) => Err(reason),
    }
}

/// Check the route, then load (size and type checked before the body is
/// read), strip metadata, and store the image as an artifact. Shared with
/// the `read` tool.
pub(crate) fn store_image(
    ctx: &ToolContext,
    raw: &str,
) -> Result<(ImageRef, image::LoadedImage, String), String> {
    let path = ctx.resolve(raw).map_err(|result| result.content)?;
    if let Route::Refused(reason) = ctx.vision.route() {
        return Err(reason);
    }
    let display = display_path(&ctx.cwd, &path);
    let loaded = image::load(&path, ctx.vision.max_bytes, ctx.vision.keep_exif)
        .map_err(|error| error.to_string())?;
    let id = ctx
        .artifacts
        .write_image(&loaded.bytes, loaded.format.extension())
        .map_err(|error| format!("cannot store the image: {error}"))?;
    let image_ref = ImageRef {
        id,
        mime: loaded.format.mime().to_string(),
        width: loaded.width,
        height: loaded.height,
        bytes: loaded.bytes.len() as u64,
    };
    Ok((image_ref, loaded, display))
}

fn image_data(loaded: &image::LoadedImage, label: &str) -> ImageData {
    crate::media::vision::image_data(loaded, label)
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

    #[tokio::test]
    async fn attaches_existing_files_and_records_the_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("bin.dat"), [0u8, 1, 2]).unwrap();
        let ctx = test_context(dir.path());

        let resolved = resolve(&ctx, "look at @a.rs and @bin.dat, ping @someone", 10_000).await;
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
    fn named_paths_are_found_in_prose_and_stack_traces() {
        assert_eq!(
            parse_named(
                "the panic at src/parser.rs:88:5 and (see README.md). Also `lib/util.py`, \
                 File \"pkg/mod.py\", line 3 and src/a.ts(12,5): error"
            ),
            vec![
                "src/parser.rs",
                "README.md",
                "lib/util.py",
                "pkg/mod.py",
                "src/a.ts"
            ]
        );
        // Not paths: versions, numbers, URLs, options, home paths, @mentions.
        assert!(
            parse_named(
                "bump to v1.2 or 3.14, see https://x.io/a.rs --out=a.txt ~/a.rs @b.rs e.g."
            )
            .iter()
            .all(|word| word == "e.g"),
            "{:?}",
            parse_named(
                "bump to v1.2 or 3.14, see https://x.io/a.rs --out=a.txt ~/a.rs @b.rs e.g."
            )
        );
    }

    #[tokio::test]
    async fn files_named_without_at_are_attached_only_when_they_exist_here() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/parser.rs"), "fn parse() {}\n").unwrap();
        std::fs::write(dir.path().join("blob.bin"), [0u8, 1, 2]).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("x.rs"), "fn x() {}\n").unwrap();
        let ctx = test_context(dir.path());
        let text = format!(
            "it panics at src/parser.rs:3:1, not in missing.rs or blob.bin or {}",
            outside.path().join("x.rs").display()
        );

        let resolved = resolve_with(&ctx, &text, 10_000, true).await;
        let paths: Vec<&str> = resolved
            .attachments
            .iter()
            .map(|a| a.path.as_str())
            .collect();
        assert_eq!(paths, vec!["src/parser.rs"]);
        assert!(
            resolved.skipped.is_empty(),
            "named paths are never reported: {:?}",
            resolved.skipped
        );
        assert!(
            crate::tools::write::lock_reads(&ctx).is_current(&dir.path().join("src/parser.rs"))
        );

        // Off: only @ mentions attach.
        let ctx = test_context(dir.path());
        assert!(
            resolve_with(&ctx, &text, 10_000, false)
                .await
                .attachments
                .is_empty()
        );
    }

    #[tokio::test]
    async fn named_files_are_capped_and_never_duplicate_a_mention() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"] {
            std::fs::write(dir.path().join(name), "x\n").unwrap();
        }
        let ctx = test_context(dir.path());
        let resolved =
            resolve_with(&ctx, "@a.rs then a.rs b.rs c.rs d.rs e.rs", 10_000, true).await;
        let paths: Vec<&str> = resolved
            .attachments
            .iter()
            .map(|a| a.path.as_str())
            .collect();
        assert_eq!(paths, vec!["a.rs", "b.rs", "c.rs", "d.rs"]);
    }

    #[tokio::test]
    async fn respects_the_character_budget() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.txt"), "x".repeat(500)).unwrap();
        let ctx = test_context(dir.path());
        let resolved = resolve(&ctx, "@big.txt", 100).await;
        assert!(resolved.attachments.is_empty());
        assert!(resolved.skipped[0].contains("too large"));
    }

    fn vision_context(dir: &std::path::Path, native: bool) -> ToolContext {
        let mut ctx = test_context(dir);
        ctx.vision = std::sync::Arc::new(crate::media::VisionSettings {
            native,
            ..Default::default()
        });
        ctx
    }

    #[tokio::test]
    async fn images_attach_natively_stripped_and_stored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("shot.png"),
            crate::media::image::fixtures::png_with_metadata(),
        )
        .unwrap();
        let ctx = vision_context(dir.path(), true);
        let resolved = resolve(&ctx, "why is @shot.png broken", 100_000).await;
        assert!(resolved.skipped.is_empty(), "{:?}", resolved.skipped);
        let attachment = &resolved.attachments[0];
        let image = attachment.image.as_ref().unwrap();
        assert_eq!((image.width, image.height), (3, 2));
        assert_eq!(image.mime, "image/png");
        let stored = ctx.artifacts.read_image(&image.id).unwrap();
        assert!(!stored.windows(13).any(|w| w == b"secret person"));
    }

    /// A chat endpoint that records the request and streams one reply.
    async fn describer_endpoint(
        reply: &'static str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<String>>) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let record = seen.clone();
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move |body: String| {
                let record = record.clone();
                async move {
                    *record.lock().unwrap() = body;
                    let chunk = json!({"choices": [{"index": 0, "delta": {"content": reply}}]});
                    let stream = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        stream,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{address}"), seen)
    }

    #[tokio::test]
    async fn a_text_model_gets_a_labelled_description_from_the_vision_model() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("shot.png"),
            crate::media::image::fixtures::png_with_metadata(),
        )
        .unwrap();
        let (endpoint, seen) = describer_endpoint("A red Save button overlaps the footer.").await;
        let mut config = crate::config::Config::builtin();
        config.model.endpoint = endpoint;
        config.vision.model = "vl-model".into();
        let mut ctx = test_context(dir.path());
        ctx.vision = std::sync::Arc::new(crate::media::VisionSettings::resolve(
            &config,
            Some(false),
            None,
        ));

        let resolved = resolve(&ctx, "fix @shot.png", 100_000).await;
        assert!(resolved.skipped.is_empty(), "{:?}", resolved.skipped);
        let attachment = &resolved.attachments[0];
        assert_eq!(attachment.described_by.as_deref(), Some("vl-model"));
        assert!(attachment.content.contains("red Save button"));
        let user = crate::session::UserData {
            content: "fix @shot.png".into(),
            attachments: resolved.attachments.clone(),
        };
        assert!(user.model_content().contains("described_by=\"vl-model\""));
        // The describer received the stripped image as a data URL.
        let body = seen.lock().unwrap().clone();
        assert!(body.contains("\"model\":\"vl-model\""), "{body}");
        assert!(body.contains("data:image/png;base64,"), "{body}");
    }

    #[tokio::test]
    async fn images_without_vision_or_too_large_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("shot.png"),
            crate::media::image::fixtures::png_with_metadata(),
        )
        .unwrap();
        let ctx = vision_context(dir.path(), false);
        let resolved = resolve(&ctx, "@shot.png", 100_000).await;
        assert!(resolved.attachments.is_empty());
        assert!(
            resolved.skipped[0].contains("no vision input"),
            "{:?}",
            resolved.skipped
        );

        let mut ctx = vision_context(dir.path(), true);
        std::sync::Arc::get_mut(&mut ctx.vision).unwrap().max_bytes = 8;
        let resolved = resolve(&ctx, "@shot.png", 100_000).await;
        assert!(
            resolved.skipped[0].contains("over the"),
            "{:?}",
            resolved.skipped
        );
    }
}
