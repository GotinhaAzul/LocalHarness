//! Tool-result caps and archive handles.

use crate::session::ArtifactStore;

pub const RESULT_CAP_CHARS: usize = 2500;
pub const BATCH_CAP_CHARS: usize = 8000;
pub const PREVIEW_CHARS: usize = 1800;
pub const RECALL_CAP_CHARS: usize = 4000;
/// Of a head-and-tail preview, the part kept from the start. The rest of
/// `PREVIEW_CHARS` comes from the end, where command output puts the result.
pub const PREVIEW_HEAD_CHARS: usize = 500;

/// Per-result and per-batch character caps for tool output.
///
/// They scale with the usable prompt: a fixed 2500-character cap is about 1%
/// of a 64k window, so on a large window the model paid extra `read`/`recall`
/// round trips for text that would have fit. Small windows keep the original
/// values (the floor of each clamp).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResultCaps {
    /// Characters one tool result may occupy in context.
    pub result: usize,
    /// Characters all results of one batch may occupy together.
    pub batch: usize,
}

impl Default for ResultCaps {
    fn default() -> Self {
        Self {
            result: RESULT_CAP_CHARS,
            batch: BATCH_CAP_CHARS,
        }
    }
}

/// Upper clamp of the scaled per-result cap.
pub const RESULT_CAP_MAX_CHARS: usize = 16_000;
/// Upper clamp of the scaled per-batch cap.
pub const BATCH_CAP_MAX_CHARS: usize = 40_000;
/// Room a self-sizing tool leaves for the `[tool] name - args` header, notes
/// and hook text the loop adds around its output.
pub const TOOL_HEADROOM_CHARS: usize = 300;

impl ResultCaps {
    /// Caps for a usable prompt of `tokens`: 4% and 12% of it in characters
    /// (at the default prose density), clamped to 2500..16000 and
    /// 8000..40000. The default density is used, not the calibrated one, so
    /// the caps do not move inside a session.
    pub fn for_usable_prompt(tokens: u32) -> Self {
        let chars = f64::from(tokens) * crate::context::budget::DEFAULT_CHARS_PER_TOKEN;
        let result = ((chars * 0.04) as usize).clamp(RESULT_CAP_CHARS, RESULT_CAP_MAX_CHARS);
        let batch = ((chars * 0.12) as usize).clamp(BATCH_CAP_CHARS, BATCH_CAP_MAX_CHARS);
        Self { result, batch }
    }

    /// Apply explicit overrides from config.
    pub fn with_overrides(mut self, result: Option<usize>, batch: Option<usize>) -> Self {
        if let Some(result) = result {
            self.result = result.max(500);
        }
        if let Some(batch) = batch {
            self.batch = batch.max(self.result);
        }
        self
    }

    /// Preview size for an oversized result (the same 18/25 share as the
    /// original 1800 of 2500).
    pub fn preview(&self) -> usize {
        self.result * PREVIEW_CHARS / RESULT_CAP_CHARS
    }

    /// Characters a tool that sizes its own output (`read`, `grep`, `glob`)
    /// may fill so the loop never has to cut it blindly.
    pub fn tool_budget(&self) -> usize {
        self.result.saturating_sub(TOOL_HEADROOM_CHARS).max(400)
    }
}

#[derive(Debug, Clone)]
pub struct CappedContent {
    pub content: String,
    pub artifact: Option<String>,
    pub original_chars: usize,
    pub truncated: bool,
}

fn split_preview(content: &str, max_chars: usize) -> &str {
    match content.char_indices().nth(max_chars) {
        Some((index, _)) => &content[..index],
        None => content,
    }
}

/// Which part of an oversized result the model sees inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preview {
    /// The start (files, search results).
    Head,
    /// The start and the end, with the omitted lines named for `recall`
    /// (command output, where errors and summaries come last).
    HeadTail,
}

/// Cap one tool result. Oversized content is archived in full and replaced by
/// a preview plus the artifact handle and size.
pub fn cap_result(store: &ArtifactStore, content: &str) -> CappedContent {
    cap_result_with(store, content, Preview::Head)
}

/// [`cap_result`] with a choice of preview.
pub fn cap_result_with(store: &ArtifactStore, content: &str, preview: Preview) -> CappedContent {
    cap_result_limited(store, content, preview, ResultCaps::default())
}

/// [`cap_result_with`] under explicit caps.
pub fn cap_result_limited(
    store: &ArtifactStore,
    content: &str,
    preview: Preview,
    caps: ResultCaps,
) -> CappedContent {
    let original_chars = content.chars().count();
    let preview_chars = caps.preview();
    if original_chars <= caps.result {
        return CappedContent {
            content: content.to_string(),
            artifact: None,
            original_chars,
            truncated: false,
        };
    }
    match store.write(content) {
        Ok(artifact) if preview == Preview::HeadTail => {
            let capped = head_tail_preview(content, &artifact.id, artifact.bytes, preview_chars);
            CappedContent {
                content: capped,
                artifact: Some(artifact.id),
                original_chars,
                truncated: true,
            }
        }
        Ok(artifact) => CappedContent {
            content: head_preview(content, &artifact.id, artifact.bytes, preview_chars),
            artifact: Some(artifact.id),
            original_chars,
            truncated: true,
        },
        Err(err) => {
            tracing::warn!(%err, "cannot archive tool result; truncating in place");
            CappedContent {
                content: split_preview(content, caps.result).to_string(),
                artifact: None,
                original_chars,
                truncated: true,
            }
        }
    }
}

/// The first whole lines of `content` within `PREVIEW_CHARS`, with the first
/// line that was left out named for `recall`. Cutting inside a line shows the
/// model half a statement, and a hint that always starts at line 1 sends it
/// back for text it already has.
fn head_preview(content: &str, id: &str, bytes: usize, preview_chars: usize) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let mut shown = 0;
    let mut used = 0;
    while shown < lines.len() {
        let width = lines[shown].chars().count() + 1;
        if used + width > preview_chars {
            break;
        }
        used += width;
        shown += 1;
    }
    if shown == 0 || shown == lines.len() {
        // One line longer than the preview (or nothing to continue from):
        // fall back to a plain character preview.
        return format!(
            "{}\n\n(truncated; recall {id} 1-120 · full artifact {bytes} bytes)",
            split_preview(content, preview_chars)
        );
    }
    let mut out = String::with_capacity(used + 120);
    for line in &lines[..shown] {
        out.push_str(line);
        out.push('\n');
    }
    let next = shown + 1;
    out.push_str(&format!(
        "\n(truncated after line {shown} of {}; recall {id} {next}-{} · full artifact {bytes} bytes)",
        lines.len(),
        lines.len().min(shown + 120)
    ));
    out
}

/// The first lines and the last lines of `content` within `PREVIEW_CHARS`,
/// cut at line boundaries, with the omitted line range named so `recall` can
/// fetch exactly what was skipped.
fn head_tail_preview(content: &str, id: &str, bytes: usize, preview_chars: usize) -> String {
    let head_chars = preview_chars * PREVIEW_HEAD_CHARS / PREVIEW_CHARS;
    let lines: Vec<&str> = content.lines().collect();
    let width = |line: &&str| line.chars().count() + 1;
    // Whole lines from the start within the head budget (at least one, cut
    // if a single line is longer than the budget).
    let mut head = 0;
    let mut used = 0;
    while head < lines.len() && used + width(&lines[head]) <= head_chars {
        used += width(&lines[head]);
        head += 1;
    }
    // Whole lines from the end within what is left.
    let tail_budget = preview_chars.saturating_sub(used);
    let mut tail_start = lines.len();
    let mut tail_used = 0;
    while tail_start > head && tail_used + width(&lines[tail_start - 1]) <= tail_budget {
        tail_used += width(&lines[tail_start - 1]);
        tail_start -= 1;
    }
    if head == 0 && tail_start == lines.len() {
        // One enormous line: fall back to a plain character preview.
        return format!(
            "{}\n\n(truncated; recall {id} 1-120 · full artifact {bytes} bytes)",
            split_preview(content, preview_chars)
        );
    }
    let mut out = String::with_capacity(preview_chars + 200);
    for line in &lines[..head] {
        out.push_str(line);
        out.push('\n');
    }
    if tail_start > head {
        // 1-based line numbers of the gap.
        let (first, last) = (head + 1, tail_start);
        out.push_str(&format!(
            "… {} lines omitted ({first}-{last}); recall {id} {first}-{} …\n",
            last - first + 1,
            last.min(first + 119)
        ));
    }
    for line in &lines[tail_start..] {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!(
        "\n(truncated; full artifact {id} · {bytes} bytes · {} lines)",
        lines.len()
    ));
    out
}

/// Apply the per-result cap and then enforce a combined batch cap. Results
/// beyond the batch cap are archived whole so no output is lost.
pub fn cap_batch(store: &ArtifactStore, results: &mut [CappedContent]) {
    let mut used = 0usize;
    for result in results.iter_mut() {
        let len = result.content.chars().count();
        if used + len <= BATCH_CAP_CHARS {
            used += len;
            continue;
        }
        let remaining = BATCH_CAP_CHARS.saturating_sub(used);
        if remaining < 200 {
            let artifact = store.write(&result.content).ok();
            result.content = format!(
                "(batch cap reached; {len} chars archived{})",
                artifact
                    .as_ref()
                    .map(|a| format!(" as {}", a.id))
                    .unwrap_or_default()
            );
            if let Some(artifact) = artifact {
                result.artifact = Some(artifact.id);
            }
            result.truncated = true;
        } else {
            let preview: String = result.content.chars().take(remaining).collect();
            let artifact = store.write(&result.content).ok();
            result.content = format!(
                "{preview}\n\n(batch cap; recall {}{})",
                artifact.as_ref().map(|a| a.id.as_str()).unwrap_or("failed"),
                artifact
                    .as_ref()
                    .map(|a| format!(" 1-120 · {} bytes", a.bytes))
                    .unwrap_or_default()
            );
            if let Some(artifact) = artifact {
                result.artifact = Some(artifact.id);
            }
            result.truncated = true;
            used = BATCH_CAP_CHARS;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_results_are_untouched() {
        let store = ArtifactStore::ephemeral();
        let capped = cap_result(&store, "hello");
        assert_eq!(capped.content, "hello");
        assert!(!capped.truncated);
        assert!(capped.artifact.is_none());
    }

    #[test]
    fn large_results_are_archived_with_handle() {
        let store = ArtifactStore::ephemeral();
        let content = "x".repeat(10_000);
        let capped = cap_result(&store, &content);
        assert!(capped.truncated);
        assert!(capped.content.len() <= RESULT_CAP_CHARS + 200);
        assert!(capped.content.contains("recall out-1 1-120"));
        let full = store.read_all(capped.artifact.as_deref().unwrap()).unwrap();
        assert_eq!(full.len(), 10_000);
    }

    #[test]
    fn caps_scale_with_the_usable_prompt() {
        use crate::context::TokenBudget;
        // Small windows keep the original fixed caps.
        for window in [4096, 8192, 16_384] {
            let usable = TokenBudget::new(window, 2048).usable_prompt();
            assert_eq!(ResultCaps::for_usable_prompt(usable), ResultCaps::default());
        }
        let caps = ResultCaps::for_usable_prompt(TokenBudget::new(65_536, 8192).usable_prompt());
        assert!(caps.result > 7_000 && caps.result < 8_000, "{caps:?}");
        assert!(caps.batch > 21_000 && caps.batch < 24_000, "{caps:?}");
        // Very large windows stop at the clamp.
        let huge = ResultCaps::for_usable_prompt(1_000_000);
        assert_eq!(huge.result, RESULT_CAP_MAX_CHARS);
        assert_eq!(huge.batch, BATCH_CAP_MAX_CHARS);
        assert_eq!(ResultCaps::default().preview(), PREVIEW_CHARS);
        let forced = ResultCaps::default().with_overrides(Some(6000), Some(1000));
        assert_eq!(forced.result, 6000);
        assert_eq!(forced.batch, 6000);
    }

    #[test]
    fn a_larger_cap_keeps_more_inline() {
        let store = ArtifactStore::ephemeral();
        let content: String = (1..=200)
            .map(|n| format!("line {n:>4} of the result\n"))
            .collect();
        let caps = ResultCaps {
            result: 8000,
            batch: 24_000,
        };
        let capped = cap_result_limited(&store, &content, Preview::Head, caps);
        assert!(!capped.truncated, "{} chars", content.chars().count());
        let bigger: String = content.repeat(3);
        let capped = cap_result_limited(&store, &bigger, Preview::Head, caps);
        assert!(capped.truncated);
        assert!(capped.content.chars().count() <= caps.result);
        assert!(capped.content.chars().count() > RESULT_CAP_CHARS);
    }

    #[test]
    fn head_preview_ends_on_a_line_and_names_the_next_one() {
        let store = ArtifactStore::ephemeral();
        let mut content = String::from("[tool] read - src/lib.rs\n");
        for n in 1..=150 {
            content.push_str(&format!(
                "{n:>3}  let value_{n} = compute_something({n});\n"
            ));
        }
        let capped = cap_result(&store, &content);
        assert!(capped.truncated);
        let body = capped.content.split("\n\n(truncated").next().unwrap();
        let shown = body.lines().count();
        // Every shown line is a whole line of the original.
        for (shown_line, original) in body.lines().zip(content.lines()) {
            assert_eq!(shown_line, original);
        }
        assert!(shown > 10 && shown < 151, "{shown}");
        let hint = format!(
            "(truncated after line {shown} of 151; recall out-1 {}-{}",
            shown + 1,
            151.min(shown + 120)
        );
        assert!(capped.content.contains(&hint), "{}", capped.content);
        assert!(capped.content.chars().count() <= RESULT_CAP_CHARS);
    }

    #[test]
    fn command_output_keeps_the_end_and_names_the_gap() {
        let store = ArtifactStore::ephemeral();
        let mut content = String::from("[tool] bash - cargo build\n");
        for n in 0..400 {
            content.push_str(&format!("   Compiling crate-{n} v0.1.0\n"));
        }
        content.push_str("error[E0308]: mismatched types\n  --> src/main.rs:3:5\n");
        let capped = cap_result_with(&store, &content, Preview::HeadTail);
        assert!(capped.truncated);
        assert!(
            capped.content.len() <= RESULT_CAP_CHARS + 200,
            "{}",
            capped.content.len()
        );
        assert!(capped.content.starts_with("[tool] bash - cargo build"));
        assert!(capped.content.contains("error[E0308]: mismatched types"));
        assert!(capped.content.contains("src/main.rs:3:5"));
        // The gap is named exactly, and recall fetches those very lines.
        let marker = capped
            .content
            .lines()
            .find(|line| line.contains("lines omitted"))
            .expect("gap marker");
        let range = marker.split("recall out-1 ").nth(1).unwrap();
        let first: usize = range.split('-').next().unwrap().parse().unwrap();
        let head_lines = capped
            .content
            .lines()
            .take_while(|line| !line.contains("lines omitted"))
            .count();
        assert_eq!(first, head_lines + 1, "the gap starts right after the head");
        let full = store.read_all("out-1").unwrap();
        let first_omitted = full.lines().nth(first - 1).unwrap();
        assert!(!capped.content.contains(first_omitted), "{first_omitted}");
    }

    #[test]
    fn head_tail_handles_one_huge_line() {
        let store = ArtifactStore::ephemeral();
        let capped = cap_result_with(&store, &"z".repeat(10_000), Preview::HeadTail);
        assert!(capped.truncated);
        assert!(capped.content.contains("recall out-1 1-120"));
        assert!(capped.content.len() <= RESULT_CAP_CHARS + 200);
    }

    #[test]
    fn batch_cap_archives_extra_results() {
        let store = ArtifactStore::ephemeral();
        let mut results: Vec<_> = (0..5)
            .map(|_| CappedContent {
                content: "y".repeat(2000),
                artifact: None,
                original_chars: 2000,
                truncated: false,
            })
            .collect();
        cap_batch(&store, &mut results);
        let total: usize = results.iter().map(|r| r.content.chars().count()).sum();
        assert!(total <= BATCH_CAP_CHARS + 500);
        assert!(results.last().unwrap().artifact.is_some());
    }
}
