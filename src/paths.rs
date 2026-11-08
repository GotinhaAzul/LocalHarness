//! Config, data, and project paths, plus the stable project id.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// All filesystem locations the harness uses.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cwd: PathBuf,
    pub project_id: String,
}

impl Paths {
    /// Discover paths for `cwd`, honoring `LH_CONFIG_DIR`/`LH_DATA_DIR`.
    pub fn discover(cwd: &Path) -> Result<Self> {
        let config_dir = match std::env::var_os("LH_CONFIG_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::config_dir()
                .context("no config directory; set LH_CONFIG_DIR")?
                .join("localharness"),
        };
        let data_dir = match std::env::var_os("LH_DATA_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::data_dir()
                .context("no data directory; set LH_DATA_DIR")?
                .join("localharness"),
        };
        let cwd = cwd
            .canonicalize()
            .with_context(|| format!("cannot resolve working directory {}", cwd.display()))?;
        Ok(Self {
            config_dir,
            data_dir,
            project_id: project_id(&cwd),
            cwd,
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn global_profiles_dir(&self) -> PathBuf {
        self.config_dir.join("profiles")
    }

    pub fn skills_dir(&self) -> PathBuf {
        self.config_dir.join("skills")
    }

    pub fn workflows_dir(&self) -> PathBuf {
        self.config_dir.join("workflows")
    }

    pub fn tools_dir(&self) -> PathBuf {
        self.config_dir.join("tools")
    }

    pub fn themes_dir(&self) -> PathBuf {
        self.config_dir.join("themes")
    }

    pub fn project_dir(&self) -> PathBuf {
        self.cwd.join(".harness")
    }

    pub fn sessions_root(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }

    pub fn project_sessions_dir(&self) -> PathBuf {
        self.sessions_root().join(&self.project_id)
    }

    pub fn session_file(&self, session_id: &str) -> PathBuf {
        self.project_sessions_dir()
            .join(format!("{session_id}.jsonl"))
    }

    pub fn artifacts_root(&self) -> PathBuf {
        self.project_sessions_dir().join("artifacts")
    }

    pub fn artifacts_dir(&self, session_id: &str) -> PathBuf {
        self.artifacts_root().join(session_id)
    }

    pub fn engagements_dir(&self) -> PathBuf {
        self.data_dir.join("engagements")
    }

    pub fn engagement_dir(&self, name: &str) -> PathBuf {
        self.engagements_dir().join(name)
    }

    pub fn log_file(&self) -> PathBuf {
        self.data_dir.join("localharness.log")
    }

    /// Create the directories the harness writes to, with `0700` permissions.
    pub fn ensure_dirs(&self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for dir in [
            self.config_dir.clone(),
            self.data_dir.clone(),
            self.sessions_root(),
            self.project_sessions_dir(),
            self.artifacts_root(),
            self.engagements_dir(),
        ] {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        Ok(())
    }
}

/// First 12 hex characters of SHA-256 over the canonical working directory.
pub fn project_id(cwd: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cwd.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(12);
    for byte in &digest[..6] {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `YYYYMMDD-HHMMSS-<pid>` session id.
pub fn new_session_id() -> String {
    chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string() + &format!("-{}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_id_is_stable_and_short() {
        let a = project_id(Path::new("/tmp/x"));
        let b = project_id(Path::new("/tmp/x"));
        assert_eq!(a, b);
        assert_eq!(a.len(), 12);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn session_id_shape() {
        let id = new_session_id();
        assert_eq!(
            id.len(),
            8 + 1 + 6 + 1 + std::process::id().to_string().len()
        );
    }
}
