//! Engagement evidence chain.
//!
//! Every command-running tool appends an immutable entry with the target,
//! command, timestamps, exit status, a content-addressed output file, and a
//! SHA-256 digest. Findings reference entry ids so a report can be audited
//! without trusting model prose.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_EVIDENCE: usize = 20_000;
const MAX_COMMAND_CHARS: usize = 2_000;
const MAX_TARGET_CHARS: usize = 500;
const MAX_SUMMARY_CHARS: usize = 300;
const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub id: String,
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    pub target: String,
    pub command: String,
    pub started: DateTime<Utc>,
    pub finished: DateTime<Utc>,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub is_error: bool,
    /// Path relative to the engagement directory.
    pub artifact: String,
    pub sha256: String,
    pub bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewEvidence {
    pub tool: String,
    pub session: Option<String>,
    pub entry: Option<String>,
    pub target: String,
    pub command: String,
    pub started: DateTime<Utc>,
    pub finished: DateTime<Utc>,
    pub duration_ms: u64,
    pub exit_code: Option<i64>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub is_error: bool,
    pub output: String,
    pub summary: Option<String>,
}

#[derive(Debug)]
pub struct EvidenceLedger {
    dir: PathBuf,
    path: PathBuf,
    entries: Vec<Evidence>,
    next_id: u32,
}

impl EvidenceLedger {
    pub fn open(engagement_dir: &Path) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;

        let evidence_dir = engagement_dir.join("evidence");
        std::fs::create_dir_all(&evidence_dir)
            .with_context(|| format!("cannot create {}", evidence_dir.display()))?;
        let _ = std::fs::set_permissions(&evidence_dir, std::fs::Permissions::from_mode(0o700));
        let path = engagement_dir.join("evidence.jsonl");
        let mut entries: Vec<Evidence> = Vec::new();
        let mut next_id = 1u32;
        if path.exists() {
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            use std::io::BufRead;
            let file = std::fs::File::open(&path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            for line in std::io::BufReader::new(file).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let entry: Evidence = serde_json::from_str(&line)
                    .with_context(|| format!("invalid evidence entry in {}", path.display()))?;
                if let Some(number) = entry
                    .id
                    .strip_prefix('e')
                    .and_then(|number| number.parse::<u32>().ok())
                {
                    next_id = next_id.max(number.saturating_add(1));
                }
                entries.push(entry);
            }
        }
        Ok(Self {
            dir: engagement_dir.to_path_buf(),
            path,
            entries,
            next_id,
        })
    }

    pub fn list(&self) -> &[Evidence] {
        &self.entries
    }

    pub fn get(&self, id: &str) -> Option<&Evidence> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    pub fn last_for_entry(&self, session: &str, entry: &str) -> Option<&Evidence> {
        self.entries.iter().rev().find(|evidence| {
            evidence.session.as_deref() == Some(session) && evidence.entry.as_deref() == Some(entry)
        })
    }

    pub fn artifact_path(&self, evidence: &Evidence) -> PathBuf {
        self.dir.join(&evidence.artifact)
    }

    pub fn read(&self, id: &str) -> Result<(Evidence, String)> {
        let evidence = self
            .get(id)
            .with_context(|| format!("no evidence {id:?}; `evidence list` shows ids"))?
            .clone();
        let path = self.artifact_path(&evidence);
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Ok((evidence, content))
    }

    pub fn verify(&self, id: &str) -> Result<(Evidence, bool)> {
        let (evidence, content) = self.read(id)?;
        let digest = sha256_hex(content.as_bytes());
        let matches = digest == evidence.sha256;
        Ok((evidence, matches))
    }

    pub fn add(&mut self, new: NewEvidence) -> Result<Evidence> {
        use std::os::unix::fs::PermissionsExt;

        if self.entries.len() >= MAX_EVIDENCE {
            anyhow::bail!(
                "engagement has {MAX_EVIDENCE} evidence entries; start a new engagement for a new phase"
            );
        }
        if new.output.len() > MAX_OUTPUT_BYTES {
            anyhow::bail!(
                "evidence output is {} bytes; the limit is {MAX_OUTPUT_BYTES}",
                new.output.len()
            );
        }
        let id = format!("e{}", self.next_id);
        let artifact_name = format!("{id}.txt");
        let artifact_path = self.dir.join("evidence").join(&artifact_name);
        write_private(&artifact_path, new.output.as_bytes())?;
        let sha256 = sha256_hex(new.output.as_bytes());
        let entry = Evidence {
            id: id.clone(),
            tool: clip(&new.tool, 80),
            session: new.session,
            entry: new.entry,
            target: clip(&new.target, MAX_TARGET_CHARS),
            command: clip(&new.command, MAX_COMMAND_CHARS),
            started: new.started,
            finished: new.finished,
            duration_ms: new.duration_ms,
            exit_code: new.exit_code,
            timed_out: new.timed_out,
            cancelled: new.cancelled,
            is_error: new.is_error,
            artifact: format!("evidence/{artifact_name}"),
            sha256,
            bytes: new.output.len(),
            summary: new.summary.map(|summary| clip(&summary, MAX_SUMMARY_CHARS)),
        };
        self.append(&entry)?;
        self.entries.push(entry.clone());
        self.next_id += 1;
        let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        Ok(entry)
    }

    fn append(&self, entry: &Evidence) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;

        let line = serde_json::to_string(entry).context("cannot serialize evidence")?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)
            .with_context(|| format!("cannot append {}", self.path.display()))?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        Ok(())
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, bytes).with_context(|| format!("cannot write {}", temp.display()))?;
    std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&temp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_evidence(output: &str) -> NewEvidence {
        let now = Utc::now();
        NewEvidence {
            tool: "bash".into(),
            session: Some("s1".into()),
            entry: Some("t1".into()),
            target: "example.com".into(),
            command: "nmap example.com".into(),
            started: now,
            finished: now,
            duration_ms: 12,
            exit_code: Some(0),
            timed_out: false,
            cancelled: false,
            is_error: false,
            output: output.into(),
            summary: None,
        }
    }

    #[test]
    fn evidence_roundtrip_hashes_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = EvidenceLedger::open(dir.path()).unwrap();
        let evidence = ledger.add(new_evidence("PORT 443 open\n")).unwrap();
        assert_eq!(evidence.id, "e1");
        assert_eq!(evidence.sha256, sha256_hex(b"PORT 443 open\n"));
        let (_, content) = ledger.read("e1").unwrap();
        assert_eq!(content, "PORT 443 open\n");
        assert!(ledger.verify("e1").unwrap().1);
        assert_eq!(
            ledger
                .last_for_entry("s1", "t1")
                .map(|entry| entry.id.as_str()),
            Some("e1")
        );

        let reloaded = EvidenceLedger::open(dir.path()).unwrap();
        assert_eq!(reloaded.list().len(), 1);
        assert_eq!(reloaded.list()[0].id, "e1");
    }

    #[test]
    fn evidence_artifact_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let mut ledger = EvidenceLedger::open(dir.path()).unwrap();
        ledger.add(new_evidence("secret output\n")).unwrap();
        let path = dir.path().join("evidence/e1.txt");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn verify_detects_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = EvidenceLedger::open(dir.path()).unwrap();
        ledger.add(new_evidence("original\n")).unwrap();
        std::fs::write(dir.path().join("evidence/e1.txt"), "changed\n").unwrap();
        assert!(!ledger.verify("e1").unwrap().1);
    }

    #[test]
    fn ids_continue_after_reload() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut ledger = EvidenceLedger::open(dir.path()).unwrap();
            ledger.add(new_evidence("one\n")).unwrap();
            ledger.add(new_evidence("two\n")).unwrap();
        }
        let mut ledger = EvidenceLedger::open(dir.path()).unwrap();
        let evidence = ledger.add(new_evidence("three\n")).unwrap();
        assert_eq!(evidence.id, "e3");
    }
}
