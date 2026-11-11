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

use super::redact::Redactor;

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
    /// Digest of the raw command output, before redaction. This keeps
    /// evidence comparable with the output the tool actually returned.
    pub sha256: String,
    /// Digest of the bytes stored in `artifact` (the redacted form). Absent in
    /// entries written before redaction existed, where it equals `sha256`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stored_sha256: Option<String>,
    /// True when at least one value was redacted before the artifact and
    /// metadata were written.
    #[serde(default)]
    pub redacted: bool,
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
    redactor: Redactor,
}

impl EvidenceLedger {
    pub fn open(engagement_dir: &Path) -> Result<Self> {
        Self::open_with(engagement_dir, true)
    }

    pub fn open_with(engagement_dir: &Path, redact: bool) -> Result<Self> {
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
            redactor: Redactor::new(redact),
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
        // Entries written before redaction have no stored digest; their
        // artifact is the raw output and `sha256` still describes it.
        let expected = evidence
            .stored_sha256
            .as_deref()
            .unwrap_or(&evidence.sha256);
        let matches = digest == expected;
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
        // The on-disk artifact and metadata are redacted, but the digest keeps
        // describing the raw output the tool returned.
        let output = self.redactor.redact(&new.output);
        let redacted_target = self.redactor.redact_owned(&new.target);
        let redacted_command = self.redactor.redact_owned(&new.command);
        let redacted_summary = new
            .summary
            .as_deref()
            .map(|summary| self.redactor.redact_owned(summary));
        let redacted = output.as_ref() != new.output
            || redacted_target != new.target
            || redacted_command != new.command
            || redacted_summary.as_deref() != new.summary.as_deref();
        let target = clip(&redacted_target, MAX_TARGET_CHARS);
        let command = clip(&redacted_command, MAX_COMMAND_CHARS);
        let summary = redacted_summary.map(|summary| clip(&summary, MAX_SUMMARY_CHARS));
        write_private(&artifact_path, output.as_bytes())?;
        let sha256 = sha256_hex(new.output.as_bytes());
        let stored_sha256 = sha256_hex(output.as_bytes());
        let entry = Evidence {
            id: id.clone(),
            tool: clip(&new.tool, 80),
            session: new.session,
            entry: new.entry,
            target,
            command,
            started: new.started,
            finished: new.finished,
            duration_ms: new.duration_ms,
            exit_code: new.exit_code,
            timed_out: new.timed_out,
            cancelled: new.cancelled,
            is_error: new.is_error,
            artifact: format!("evidence/{artifact_name}"),
            sha256,
            stored_sha256: Some(stored_sha256),
            redacted,
            bytes: new.output.len(),
            summary,
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
    fn output_is_redacted_but_the_digest_is_the_raw_output_digest() {
        const KEY: &str = "AKIAIOSFODNN7EXAMPLE";
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = EvidenceLedger::open(dir.path()).unwrap();
        let raw = format!("aws_access_key_id = {KEY}\n");
        let evidence = ledger.add(new_evidence(&raw)).unwrap();
        assert!(evidence.redacted);
        assert_eq!(evidence.sha256, sha256_hex(raw.as_bytes()));
        assert_ne!(
            evidence.stored_sha256.as_deref(),
            Some(evidence.sha256.as_str())
        );
        let (_, content) = ledger.read("e1").unwrap();
        assert!(!content.contains(KEY), "{content}");
        assert!(content.contains("[REDACTED:aws:AKIA..."), "{content}");
        assert!(content.contains("aws_access_key_id"), "{content}");
        assert!(ledger.verify("e1").unwrap().1);
    }

    #[test]
    fn no_redact_stores_raw_output_and_still_verifies() {
        const KEY: &str = "AKIAIOSFODNN7EXAMPLE";
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = EvidenceLedger::open_with(dir.path(), false).unwrap();
        let raw = format!("{KEY}\n");
        let evidence = ledger.add(new_evidence(&raw)).unwrap();
        assert!(!evidence.redacted);
        assert_eq!(evidence.sha256, sha256_hex(raw.as_bytes()));
        let (_, content) = ledger.read("e1").unwrap();
        assert_eq!(content, raw);
        assert!(ledger.verify("e1").unwrap().1);
    }

    #[test]
    fn old_entries_without_stored_digest_still_verify() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = EvidenceLedger::open(dir.path()).unwrap();
        ledger.add(new_evidence("PORT 443 open\n")).unwrap();
        // Rewrite the line without the newer fields, like an old ledger.
        let path = dir.path().join("evidence.jsonl");
        let line = std::fs::read_to_string(&path).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        value.as_object_mut().unwrap().remove("stored_sha256");
        value.as_object_mut().unwrap().remove("redacted");
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&value).unwrap()),
        )
        .unwrap();
        let reloaded = EvidenceLedger::open(dir.path()).unwrap();
        assert!(reloaded.verify("e1").unwrap().1);
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
