//! Append-only JSONL store with an advisory lock.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;

use super::entry::Entry;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("cannot open session store {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("session {} is locked by {holder}", path.display())]
    Busy { path: PathBuf, holder: String },
    #[error("invalid entry on line {line} of {}: {message}", path.display())]
    Malformed {
        path: PathBuf,
        line: usize,
        message: String,
    },
}

pub struct SessionStore {
    path: PathBuf,
    file: File,
    lock_path: PathBuf,
    lock: File,
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore")
            .field("path", &self.path)
            .field("lock_path", &self.lock_path)
            .finish()
    }
}

impl SessionStore {
    pub fn create(path: &Path) -> Result<Self, StoreError> {
        use std::os::unix::fs::PermissionsExt;
        let io_err = |source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io_err)?;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
        let lock_path = lock_path_for(path);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(io_err)?;
        acquire(&lock, path, &lock_path)?;
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .map_err(io_err)?;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        Ok(Self {
            path: path.to_path_buf(),
            file,
            lock_path,
            lock,
        })
    }

    pub fn open(path: &Path) -> Result<(Self, Vec<Entry>), StoreError> {
        let io_err = |source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        };
        let lock_path = lock_path_for(path);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(io_err)?;
        acquire(&lock, path, &lock_path)?;
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(path)
            .map_err(io_err)?;
        let entries = read_entries(path)?;
        Ok((
            Self {
                path: path.to_path_buf(),
                file,
                lock_path,
                lock,
            },
            entries,
        ))
    }

    pub fn append(&mut self, entry: &Entry) -> Result<(), StoreError> {
        let mut line = serde_json::to_string(entry).expect("entry serializes");
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .and_then(|_| self.file.flush())
            .map_err(|source| StoreError::Io {
                path: self.path.clone(),
                source,
            })
    }

    pub fn sync(&mut self) -> Result<(), StoreError> {
        self.file.sync_data().map_err(|source| StoreError::Io {
            path: self.path.clone(),
            source,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SessionStore {
    fn drop(&mut self) {
        let _ = self.lock.unlock();
        let _ = &self.lock_path;
    }
}

fn lock_path_for(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

fn acquire(lock: &File, path: &Path, lock_path: &Path) -> Result<(), StoreError> {
    match lock.try_lock_exclusive() {
        Ok(()) => {
            let holder = format!("pid {}\n", std::process::id());
            let mut file = lock.try_clone().map_err(|source| StoreError::Io {
                path: path.to_path_buf(),
                source,
            })?;
            let _ = file.set_len(0);
            let _ = file.write_all(holder.as_bytes());
            let _ = file.flush();
            let _ = lock_path;
            Ok(())
        }
        Err(source) if source.kind() == std::io::ErrorKind::WouldBlock => {
            let mut holder = String::new();
            if let Ok(mut file) = File::open(lock_path) {
                let _ = file.read_to_string(&mut holder);
            }
            let holder = holder.trim().to_string();
            let holder = if holder.is_empty() {
                "another localharness process".to_string()
            } else {
                format!("localharness ({holder})")
            };
            Err(StoreError::Busy {
                path: path.to_path_buf(),
                holder,
            })
        }
        Err(source) => Err(StoreError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Parse a session file. A malformed final line is ignored; other malformed
/// lines are skipped with a warning.
pub fn read_entries(path: &Path) -> Result<Vec<Entry>, StoreError> {
    let file = File::open(path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut entries = Vec::new();
    let reader = BufReader::new(file);
    let lines: Vec<_> = reader.lines().collect();
    let total = lines.len();
    for (i, line) in lines.into_iter().enumerate() {
        let line = match line {
            Ok(line) => line,
            Err(source) => {
                return Err(StoreError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Entry>(trimmed) {
            Ok(entry) => entries.push(entry),
            Err(err) => {
                let is_last = i + 1 == total;
                if is_last {
                    tracing::warn!(line = i + 1, %err, "ignoring malformed final session line");
                } else {
                    tracing::warn!(line = i + 1, %err, "skipping malformed session line");
                }
            }
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::entry::{EntryKind, UserData};
    use chrono::Utc;

    fn entry(seq: u64, parent: Option<&str>, content: &str) -> Entry {
        Entry {
            v: 1,
            seq,
            id: format!("e{seq}"),
            parent: parent.map(str::to_string),
            ts: Utc::now(),
            kind: EntryKind::User(UserData {
                content: content.into(),
            }),
        }
    }

    #[test]
    fn append_open_and_ignore_bad_final_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        {
            let mut store = SessionStore::create(&path).unwrap();
            store.append(&entry(1, None, "hello")).unwrap();
            store.sync().unwrap();
        }
        {
            let mut store = SessionStore::open(&path).unwrap().0;
            store.append(&entry(2, Some("e1"), "world")).unwrap();
        }
        {
            use std::io::Write;
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            f.write_all(b"{\"truncated\":").unwrap();
        }
        let (_store, entries) = SessionStore::open(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].parent.as_deref(), Some("e1"));
    }

    #[test]
    fn second_writer_is_refused_with_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let _first = SessionStore::create(&path).unwrap();
        let err = SessionStore::open(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("locked by"), "{msg}");
        assert!(msg.contains(&std::process::id().to_string()), "{msg}");
    }

    #[test]
    fn create_does_not_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let first = SessionStore::create(&path).unwrap();
        drop(first);
        let err = SessionStore::create(&path).unwrap_err();
        assert!(matches!(err, StoreError::Io { .. }));
    }
}
