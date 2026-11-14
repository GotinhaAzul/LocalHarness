//! Artifact storage: capped tool output written to `artifacts/<session>/out-N.txt`,
//! attached images as `img-N.<ext>`, or both kept in memory when sessions are
//! disabled.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::security::redact::Redactor;

#[derive(Debug, Clone)]
pub struct Artifact {
    pub id: String,
    pub bytes: usize,
}

#[derive(Debug, Clone)]
pub struct ArtifactSlice {
    pub content: String,
    pub total_lines: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("artifact {0} does not exist")]
    NotFound(String),
    #[error("artifact id {0:?} is not valid")]
    InvalidId(String),
    #[error("cannot read artifact {id}: {source}")]
    Io {
        id: String,
        #[source]
        source: io::Error,
    },
}

/// Artifacts for one session. `root = None` keeps them in memory so
/// `--no-session` writes nothing to disk.
#[derive(Debug)]
pub struct ArtifactStore {
    session_id: String,
    root: Option<PathBuf>,
    counter: AtomicU64,
    memory: Mutex<HashMap<String, String>>,
    image_counter: AtomicU64,
    /// Image bytes: the only copy without a root, a read cache with one.
    images: Mutex<HashMap<String, Arc<Vec<u8>>>>,
    redactor: Redactor,
}

impl ArtifactStore {
    pub fn new(session_id: impl Into<String>, root: Option<PathBuf>) -> io::Result<Self> {
        Self::new_with(session_id, root, true)
    }

    pub fn new_with(
        session_id: impl Into<String>,
        root: Option<PathBuf>,
        redact: bool,
    ) -> io::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let session_id = session_id.into();
        let mut next = 0u64;
        let mut next_image = 0u64;
        if let Some(root) = &root {
            std::fs::create_dir_all(root)?;
            let _ = std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700));
            if let Ok(entries) = std::fs::read_dir(root) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if let Some(number) = name
                        .strip_prefix("out-")
                        .and_then(|rest| rest.strip_suffix(".txt"))
                        .and_then(|digits| digits.parse::<u64>().ok())
                    {
                        next = next.max(number);
                    }
                    if let Some(number) = name
                        .strip_prefix("img-")
                        .and_then(|rest| rest.split('.').next())
                        .and_then(|digits| digits.parse::<u64>().ok())
                    {
                        next_image = next_image.max(number);
                    }
                }
            }
        }
        Ok(Self {
            session_id,
            root,
            counter: AtomicU64::new(next),
            memory: Mutex::new(HashMap::new()),
            image_counter: AtomicU64::new(next_image),
            images: Mutex::new(HashMap::new()),
            redactor: Redactor::new(redact),
        })
    }

    pub fn ephemeral() -> Self {
        Self::ephemeral_with(true)
    }

    pub fn ephemeral_with(redact: bool) -> Self {
        Self {
            session_id: "ephemeral".into(),
            root: None,
            counter: AtomicU64::new(0),
            memory: Mutex::new(HashMap::new()),
            image_counter: AtomicU64::new(0),
            images: Mutex::new(HashMap::new()),
            redactor: Redactor::new(redact),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Redact text with this session's redaction setting. Used by tools that
    /// read files outside [`ArtifactStore::write`] (for example live `bg`
    /// logs).
    pub fn redact_owned(&self, text: &str) -> String {
        self.redactor.redact_owned(text)
    }

    pub fn write(&self, content: &str) -> io::Result<Artifact> {
        let n = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        let id = format!("out-{n}");
        let content = self.redactor.redact(content);
        self.write_stored(&id, content.as_ref())?;
        Ok(Artifact {
            id,
            bytes: content.len(),
        })
    }

    pub fn write_named(&self, id: &str, content: &str) -> io::Result<()> {
        validate_id(id).map_err(io::Error::other)?;
        let content = self.redactor.redact(content);
        self.write_stored(id, content.as_ref())
    }

    fn write_stored(&self, id: &str, content: &str) -> io::Result<()> {
        match &self.root {
            Some(root) => {
                use std::io::Write;
                use std::os::unix::fs::PermissionsExt;
                let path = root.join(format!("{id}.txt"));
                let tmp = root.join(format!(".{id}.tmp"));
                let mut file = std::fs::File::create(&tmp)?;
                let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
                file.write_all(content.as_bytes())?;
                file.sync_all()?;
                std::fs::rename(&tmp, &path)?;
            }
            None => {
                self.memory
                    .lock()
                    .expect("artifact memory poisoned")
                    .insert(id.to_string(), content.to_string());
            }
        }
        Ok(())
    }

    /// Store an image (already validated and stripped) and return its id,
    /// `img-N.<extension>`. Images are not redacted: they are binary.
    pub fn write_image(&self, bytes: &[u8], extension: &str) -> io::Result<String> {
        let n = self.image_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let id = format!("img-{n}.{extension}");
        validate_image_id(&id).map_err(io::Error::other)?;
        if let Some(root) = &self.root {
            use std::io::Write;
            use std::os::unix::fs::PermissionsExt;
            let tmp = root.join(format!(".{id}.tmp"));
            let mut file = std::fs::File::create(&tmp)?;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
            file.write_all(bytes)?;
            file.sync_all()?;
            std::fs::rename(&tmp, root.join(&id))?;
        }
        self.images
            .lock()
            .expect("artifact images poisoned")
            .insert(id.clone(), Arc::new(bytes.to_vec()));
        Ok(id)
    }

    /// Bytes of a stored image, cached after the first disk read.
    pub fn read_image(&self, id: &str) -> Result<Arc<Vec<u8>>, ArtifactError> {
        validate_image_id(id).map_err(ArtifactError::InvalidId)?;
        if let Some(bytes) = self
            .images
            .lock()
            .expect("artifact images poisoned")
            .get(id)
        {
            return Ok(bytes.clone());
        }
        let Some(root) = &self.root else {
            return Err(ArtifactError::NotFound(id.to_string()));
        };
        let bytes = std::fs::read(root.join(id)).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                ArtifactError::NotFound(id.to_string())
            } else {
                ArtifactError::Io {
                    id: id.to_string(),
                    source,
                }
            }
        })?;
        let bytes = Arc::new(bytes);
        self.images
            .lock()
            .expect("artifact images poisoned")
            .insert(id.to_string(), bytes.clone());
        Ok(bytes)
    }

    /// Approximate byte size of an artifact.
    pub fn size(&self, id: &str) -> Option<usize> {
        match &self.root {
            Some(root) => std::fs::metadata(root.join(format!("{id}.txt")))
                .ok()
                .map(|m| m.len() as usize),
            None => self
                .memory
                .lock()
                .expect("artifact memory poisoned")
                .get(id)
                .map(String::len),
        }
    }

    /// Read 1-based inclusive line range, capped at `max_lines` and
    /// `max_chars`. Line numbering follows `str::lines`.
    pub fn read_lines(
        &self,
        id: &str,
        start: usize,
        end: usize,
        max_lines: usize,
        max_chars: usize,
    ) -> Result<ArtifactSlice, ArtifactError> {
        let content = self.read_all(id)?;
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let start = start.max(1);
        let end = end.max(start).min(total).min(start + max_lines - 1);
        let mut out = String::new();
        let mut truncated = false;
        if total == 0 || start > total {
            return Ok(ArtifactSlice {
                content: String::new(),
                total_lines: total,
                start_line: start,
                end_line: start.saturating_sub(1),
                truncated: false,
            });
        }
        for (i, line) in lines[start - 1..end].iter().enumerate() {
            let width = end.to_string().len();
            let numbered = format!("{:>width$}  {line}\n", start + i, width = width);
            if out.len() + numbered.len() > max_chars {
                truncated = true;
                break;
            }
            out.push_str(&numbered);
        }
        if end < total {
            truncated = true;
        }
        Ok(ArtifactSlice {
            content: out,
            total_lines: total,
            start_line: start,
            end_line: end,
            truncated,
        })
    }

    pub fn read_all(&self, id: &str) -> Result<String, ArtifactError> {
        validate_id(id).map_err(ArtifactError::InvalidId)?;
        match &self.root {
            Some(root) => {
                let path = root.join(format!("{id}.txt"));
                std::fs::read_to_string(&path).map_err(|source| {
                    if source.kind() == io::ErrorKind::NotFound {
                        ArtifactError::NotFound(id.to_string())
                    } else {
                        ArtifactError::Io {
                            id: id.to_string(),
                            source,
                        }
                    }
                })
            }
            None => self
                .memory
                .lock()
                .expect("artifact memory poisoned")
                .get(id)
                .cloned()
                .ok_or_else(|| ArtifactError::NotFound(id.to_string())),
        }
    }

    /// Remove every on-disk artifact for this session.
    pub fn clean_all(&self) -> io::Result<usize> {
        if let Ok(mut images) = self.images.lock() {
            images.clear();
        }
        let Some(root) = &self.root else {
            if let Ok(mut memory) = self.memory.lock() {
                memory.clear();
            }
            return Ok(0);
        };
        let mut removed = 0;
        let Ok(entries) = std::fs::read_dir(root) else {
            return Ok(0);
        };
        for entry in entries.flatten() {
            if std::fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Remove on-disk artifacts older than `days`. Returns the count removed.
    pub fn cleanup_older_than(&self, days: u32) -> io::Result<usize> {
        if days == 0 {
            return Ok(0);
        }
        let Some(root) = &self.root else {
            return Ok(0);
        };
        let cutoff =
            std::time::SystemTime::now() - std::time::Duration::from_secs(u64::from(days) * 86_400);
        let mut removed = 0;
        let Ok(entries) = std::fs::read_dir(root) else {
            return Ok(0);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|e| e == "txt" || e == "png" || e == "jpg" || e == "webp")
                && let Ok(meta) = entry.metadata()
                && meta.modified().is_ok_and(|m| m < cutoff)
                && std::fs::remove_file(&path).is_ok()
            {
                removed += 1;
            }
        }
        Ok(removed)
    }
}

/// Remove artifact files older than `days` under a sessions root, across all
/// session artifact directories.
pub fn cleanup_root(root: &Path, days: u32) -> io::Result<usize> {
    if days == 0 {
        return Ok(0);
    }
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(u64::from(days) * 86_400);
    let mut removed = 0;
    let Ok(sessions) = std::fs::read_dir(root) else {
        return Ok(0);
    };
    for session in sessions.flatten() {
        // `root` is the per-project artifacts directory; its children are
        // per-session directories containing out-N.txt files.
        let Ok(entries) = std::fs::read_dir(session.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata()
                && meta.is_file()
                && meta.modified().is_ok_and(|modified| modified < cutoff)
                && std::fs::remove_file(entry.path()).is_ok()
            {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// `img-<digits>.<png|jpg|webp>`.
fn validate_image_id(id: &str) -> Result<(), String> {
    let valid = id
        .strip_prefix("img-")
        .and_then(|rest| rest.split_once('.'))
        .is_some_and(|(digits, extension)| {
            !digits.is_empty()
                && digits.len() <= 20
                && digits.chars().all(|c| c.is_ascii_digit())
                && matches!(extension, "png" | "jpg" | "webp")
        });
    if valid {
        Ok(())
    } else {
        Err(format!("invalid image id {id:?}"))
    }
}

fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!("invalid artifact id {id:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_artifacts_roundtrip_and_range() {
        let store = ArtifactStore::ephemeral();
        let a = store.write("one\ntwo\nthree\nfour\n").unwrap();
        assert_eq!(a.id, "out-1");
        assert_eq!(a.bytes, 19);
        let slice = store.read_lines(&a.id, 2, 3, 10, 4000).unwrap();
        assert!(slice.content.contains("2  two"));
        assert!(slice.content.contains("3  three"));
        assert_eq!(slice.total_lines, 4);
        assert!(slice.truncated);
    }

    #[test]
    fn images_roundtrip_on_disk_and_in_memory() {
        let memory = ArtifactStore::ephemeral();
        let id = memory.write_image(b"\x89PNGdata", "png").unwrap();
        assert_eq!(id, "img-1.png");
        assert_eq!(memory.read_image(&id).unwrap().as_slice(), b"\x89PNGdata");

        let dir = tempfile::tempdir().unwrap();
        {
            let store = ArtifactStore::new("s", Some(dir.path().to_path_buf())).unwrap();
            assert_eq!(store.write_image(b"a", "jpg").unwrap(), "img-1.jpg");
        }
        // A reopened store reads from disk and does not reuse ids.
        let reopened = ArtifactStore::new("s", Some(dir.path().to_path_buf())).unwrap();
        assert_eq!(reopened.read_image("img-1.jpg").unwrap().as_slice(), b"a");
        assert_eq!(reopened.write_image(b"b", "png").unwrap(), "img-2.png");
        for bad in ["../x.png", "img-1.txt", "img-.png", "img-1.png/.."] {
            assert!(reopened.read_image(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn invalid_ids_are_rejected() {
        let store = ArtifactStore::ephemeral();
        assert!(store.read_all("../etc/passwd").is_err());
        assert!(store.read_all("nope").is_err());
    }

    #[test]
    fn resumed_store_does_not_reuse_artifact_ids() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("s1");
        {
            let store = ArtifactStore::new("s1", Some(root.clone())).unwrap();
            assert_eq!(store.write("one").unwrap().id, "out-1");
            assert_eq!(store.write("two").unwrap().id, "out-2");
        }
        let reopened = ArtifactStore::new("s1", Some(root)).unwrap();
        assert_eq!(reopened.write("three").unwrap().id, "out-3");
    }

    #[test]
    fn disk_artifacts_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new("s1", Some(dir.path().join("s1"))).unwrap();
        let a = store.write("secret").unwrap();
        let path = store.root().unwrap().join(format!("{}.txt", a.id));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(store.read_all(&a.id).unwrap(), "secret");
    }

    #[test]
    fn artifacts_are_redacted_before_storage() {
        const KEY: &str = "AKIAIOSFODNN7EXAMPLE";
        let store = ArtifactStore::ephemeral();
        let a = store
            .write(&format!("aws_access_key_id = {KEY}\n"))
            .unwrap();
        let content = store.read_all(&a.id).unwrap();
        assert!(!content.contains(KEY), "{content}");
        assert!(content.contains("[REDACTED:aws:AKIA..."), "{content}");
        assert!(content.contains("aws_access_key_id"), "{content}");
    }

    #[test]
    fn no_redact_keeps_raw_artifact_values() {
        const KEY: &str = "AKIAIOSFODNN7EXAMPLE";
        let store = ArtifactStore::ephemeral_with(false);
        let raw = format!("{KEY}\n");
        let a = store.write(&raw).unwrap();
        assert_eq!(store.read_all(&a.id).unwrap(), raw);
    }
}
