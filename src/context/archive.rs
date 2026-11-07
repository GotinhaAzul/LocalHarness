//! Tool-result caps and archive handles.

use crate::session::ArtifactStore;

pub const RESULT_CAP_CHARS: usize = 2500;
pub const BATCH_CAP_CHARS: usize = 8000;
pub const PREVIEW_CHARS: usize = 1800;
pub const RECALL_CAP_CHARS: usize = 4000;

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

/// Cap one tool result. Oversized content is archived in full and replaced by
/// a preview plus the artifact handle and size.
pub fn cap_result(store: &ArtifactStore, content: &str) -> CappedContent {
    let original_chars = content.chars().count();
    if original_chars <= RESULT_CAP_CHARS {
        return CappedContent {
            content: content.to_string(),
            artifact: None,
            original_chars,
            truncated: false,
        };
    }
    match store.write(content) {
        Ok(artifact) => {
            let preview = split_preview(content, PREVIEW_CHARS);
            let mut capped = String::with_capacity(preview.len() + 120);
            capped.push_str(preview);
            capped.push_str(&format!(
                "\n\n(truncated; recall {} 1-120 · full artifact {} bytes)",
                artifact.id, artifact.bytes
            ));
            CappedContent {
                content: capped,
                artifact: Some(artifact.id),
                original_chars,
                truncated: true,
            }
        }
        Err(err) => {
            tracing::warn!(%err, "cannot archive tool result; truncating in place");
            CappedContent {
                content: split_preview(content, RESULT_CAP_CHARS).to_string(),
                artifact: None,
                original_chars,
                truncated: true,
            }
        }
    }
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
