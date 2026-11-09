//! Security pack state: engagement scope, findings, reporting.
//!
//! Security is a first-class posture: scope is user-set only, the scope gate is
//! a speed bump rather than a sandbox, and findings are engagement-global.

pub mod detect;
pub mod evidence;
pub mod findings;
pub mod graph;
pub mod report;
pub mod scope;
pub mod surface;
pub mod verify;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::paths::Paths;
pub use findings::{Engagement, EvidenceRef, Finding, FindingPatch, NewFinding, Severity, Status};
pub use report::render as render_report;
pub use scope::{Scope, ScopeViolation, extract_targets, gate_command, is_security_command};

/// Per-session security state. The scope file is shared per engagement.
#[derive(Debug, Default)]
pub struct SecurityState {
    pub engagement_name: String,
    pub scope: Scope,
    pub engagement: Option<Engagement>,
}

impl SecurityState {
    pub fn load(paths: &Paths, engagement: Option<String>) -> Result<Self> {
        let name = engagement.unwrap_or_else(|| default_engagement_name(&paths.cwd));
        let scope_path = paths.engagement_dir(&name).join("scope.toml");
        let scope = if scope_path.exists() {
            let raw = std::fs::read_to_string(&scope_path)
                .with_context(|| format!("cannot read {}", scope_path.display()))?;
            toml::from_str(&raw)
                .with_context(|| format!("invalid scope file {}", scope_path.display()))?
        } else {
            Scope::default()
        };
        let engagement_dir = paths.engagement_dir(&name);
        let active = [
            "scope.toml",
            "findings.jsonl",
            "evidence.jsonl",
            "surface.json",
        ]
        .iter()
        .any(|file| engagement_dir.join(file).exists());
        let engagement = if active {
            Some(Engagement::open(paths, &name)?)
        } else {
            None
        };
        Ok(Self {
            engagement_name: name,
            scope,
            engagement,
        })
    }

    pub fn empty() -> Self {
        Self::default()
    }

    pub fn scope_path(paths: &Paths, name: &str) -> PathBuf {
        paths.engagement_dir(name).join("scope.toml")
    }

    pub fn save_scope(&self, paths: &Paths) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = paths.engagement_dir(&self.engagement_name);
        std::fs::create_dir_all(&dir)?;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        let path = Self::scope_path(paths, &self.engagement_name);
        let raw = toml::to_string_pretty(&self.scope)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, raw)?;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Load the findings ledger on first use.
    pub fn ensure_engagement(&mut self, paths: &Paths) -> Result<&mut Engagement> {
        if self.engagement.is_none() {
            self.engagement = Some(Engagement::open(paths, &self.engagement_name)?);
        }
        Ok(self.engagement.as_mut().expect("set above"))
    }

    pub fn add_scope(&mut self, paths: &Paths, target: &str) -> Result<()> {
        self.scope.add(target).map_err(anyhow::Error::msg)?;
        self.save_scope(paths)?;
        self.ensure_engagement(paths)?;
        Ok(())
    }

    pub fn remove_scope(&mut self, paths: &Paths, target: &str) -> Result<bool> {
        let removed = self.scope.remove(target);
        if removed {
            self.save_scope(paths)?;
        }
        Ok(removed)
    }

    pub fn clear_scope(&mut self, paths: &Paths) -> Result<()> {
        self.scope.clear();
        self.save_scope(paths)
    }

    /// Scope gate used by `bash`, `bg`, and external tools.
    pub fn gate(&self, command: &str) -> Result<(), String> {
        gate_command(command, &self.scope)
    }

    pub fn summary(&self) -> String {
        let scope = if self.scope.targets.is_empty() {
            "scope: none".to_string()
        } else {
            format!("scope: {}", self.scope.targets.join(", "))
        };
        let findings = match &self.engagement {
            Some(engagement) => {
                let (open, high, _) = engagement.counts();
                format!(
                    "findings: {} open ({} high/critical) of {}\nevidence: {} entries\nsurface: {} assets, {} services",
                    open,
                    high,
                    engagement.findings.len(),
                    engagement.evidence.list().len(),
                    engagement.surface.assets().len(),
                    engagement.surface.service_count()
                )
            }
            None => "findings: none recorded\nevidence: none\nsurface: empty".to_string(),
        };
        format!("engagement: {}\n{scope}\n{findings}", self.engagement_name)
    }
}

/// Engagement directory name derived from the working directory.
pub fn default_engagement_name(cwd: &Path) -> String {
    let name = cwd
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "engagement".into());
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        "engagement".into()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(dir: &Path) -> Paths {
        let mut paths = Paths::discover(dir).unwrap();
        paths.data_dir = dir.join("data");
        paths
    }

    #[test]
    fn scope_persists_and_gates() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let mut state = SecurityState::load(&paths, Some("eng".into())).unwrap();
        state.add_scope(&paths, "10.0.0.0/24").unwrap();
        assert!(state.gate("nmap 10.0.0.5").is_ok());
        assert!(state.gate("nmap 192.0.2.1").is_err());
        let reloaded = SecurityState::load(&paths, Some("eng".into())).unwrap();
        assert!(reloaded.scope.allows("10.0.0.9"));
    }

    #[test]
    fn default_name_is_clean() {
        assert_eq!(
            default_engagement_name(Path::new("/tmp/My Project")),
            "my-project"
        );
    }
}
