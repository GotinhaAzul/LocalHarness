//! Engagement findings ledger. Engagement-global, shared by all branches.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

use super::evidence::EvidenceLedger;
use super::surface::SurfaceMap;
use super::verify::{PocStep, Verification, compute_confidence};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::paths::Paths;

pub const MAX_FINDINGS: usize = 50;
pub const MAX_EVIDENCE_CHARS: usize = 2000;
pub const MAX_TEXT_CHARS: usize = 1000;
pub const MAX_TITLE_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    High,
    #[default]
    Medium,
    Low,
    Info,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Critical => "critical",
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
            Severity::Info => "info",
        }
    }
}

impl std::str::FromStr for Severity {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "critical" | "crit" => Ok(Severity::Critical),
            "high" => Ok(Severity::High),
            "medium" | "med" => Ok(Severity::Medium),
            "low" => Ok(Severity::Low),
            "info" | "informational" => Ok(Severity::Info),
            other => Err(format!(
                "unknown severity {other:?}; use critical, high, medium, low, or info"
            )),
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    #[default]
    Open,
    Fixed,
    Accepted,
    FalsePositive,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::Fixed => "fixed",
            Status::Accepted => "accepted",
            Status::FalsePositive => "false-positive",
        }
    }
}

impl std::str::FromStr for Status {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "open" => Ok(Status::Open),
            "fixed" => Ok(Status::Fixed),
            "accepted" => Ok(Status::Accepted),
            "false-positive" | "false_positive" | "fp" => Ok(Status::FalsePositive),
            other => Err(format!(
                "unknown status {other:?}; use open, fixed, accepted, or false-positive"
            )),
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EvidenceRef {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub title: String,
    pub severity: Severity,
    pub status: Status,
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_ref: Option<EvidenceRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impact: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub poc_steps: Vec<PocStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct NewFinding {
    pub title: String,
    pub severity: Severity,
    pub target: String,
    pub evidence: Option<String>,
    pub evidence_ref: Option<EvidenceRef>,
    pub impact: Option<String>,
    pub remediation: Option<String>,
    pub session_id: Option<String>,
    pub entry_id: Option<String>,
    pub poc_steps: Vec<PocStep>,
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct FindingPatch {
    pub title: Option<String>,
    pub severity: Option<Severity>,
    pub status: Option<Status>,
    pub target: Option<String>,
    pub evidence: Option<String>,
    pub impact: Option<String>,
    pub remediation: Option<String>,
    pub poc_steps: Option<Vec<PocStep>>,
    pub confidence: Option<f64>,
}

#[derive(Debug)]
pub struct Engagement {
    pub name: String,
    pub dir: PathBuf,
    pub findings: Vec<Finding>,
    pub evidence: EvidenceLedger,
    pub surface: SurfaceMap,
    next_id: u32,
}

impl Engagement {
    pub fn open(paths: &Paths, name: &str) -> anyhow::Result<Self> {
        Self::open_with(paths, name, true)
    }

    pub fn open_with(paths: &Paths, name: &str, redact: bool) -> anyhow::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let dir = paths.engagement_dir(name);
        std::fs::create_dir_all(&dir)?;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        let evidence = EvidenceLedger::open_with(&dir, redact)?;
        let surface = SurfaceMap::open(&dir)?;
        let path = dir.join("findings.jsonl");
        let mut findings: Vec<Finding> = Vec::new();
        let mut positions: BTreeMap<String, usize> = BTreeMap::new();
        if path.exists() {
            use std::io::BufRead;
            let file = std::fs::File::open(&path)?;
            for line in std::io::BufReader::new(file).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Finding>(&line) {
                    Ok(finding) => match positions.get(&finding.id) {
                        Some(&index) => findings[index] = finding,
                        None => {
                            positions.insert(finding.id.clone(), findings.len());
                            findings.push(finding);
                        }
                    },
                    Err(err) => tracing::warn!(%err, "skipping malformed finding line"),
                }
            }
        }
        let next_id = findings
            .iter()
            .filter_map(|f| f.id.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()))
            .max()
            .unwrap_or(0)
            + 1;
        Ok(Self {
            name: name.to_string(),
            dir,
            findings,
            evidence,
            surface,
            next_id,
        })
    }

    pub fn findings_path(&self) -> PathBuf {
        self.dir.join("findings.jsonl")
    }

    pub fn add(&mut self, new: NewFinding) -> Result<Finding, String> {
        if new.title.trim().is_empty() {
            return Err("finding title is empty; give a short title".into());
        }
        if new.target.trim().is_empty() {
            return Err("finding target is empty; name the asset or URL".into());
        }
        if self.findings.len() >= MAX_FINDINGS {
            return Err(format!(
                "engagement already has {MAX_FINDINGS} findings; update or close an existing finding before adding more"
            ));
        }
        let now = Utc::now();
        let finding = Finding {
            id: format!("f{}", self.next_id),
            title: clip(&new.title, MAX_TITLE_CHARS),
            severity: new.severity,
            status: Status::Open,
            target: clip(&new.target, 500),
            evidence: new.evidence.map(|e| clip(&e, MAX_EVIDENCE_CHARS)),
            evidence_ref: new.evidence_ref,
            impact: new.impact.map(|e| clip(&e, MAX_TEXT_CHARS)),
            remediation: new.remediation.map(|e| clip(&e, MAX_TEXT_CHARS)),
            created: now,
            updated: now,
            session_id: new.session_id,
            entry_id: new.entry_id,
            confidence: compute_confidence(new.confidence, &new.poc_steps),
            poc_steps: new.poc_steps,
            verification: None,
        };
        self.next_id += 1;
        self.append(&finding)?;
        self.findings.push(finding.clone());
        Ok(finding)
    }

    pub fn update(&mut self, id: &str, patch: FindingPatch) -> Result<Finding, String> {
        let Some(index) = self.findings.iter().position(|f| f.id == id) else {
            return Err(format!("no finding {id:?}; `finding list` shows ids"));
        };
        let mut finding = self.findings[index].clone();
        if let Some(title) = patch.title {
            if title.trim().is_empty() {
                return Err("finding title cannot be empty".into());
            }
            finding.title = clip(&title, MAX_TITLE_CHARS);
        }
        if let Some(severity) = patch.severity {
            finding.severity = severity;
        }
        if let Some(status) = patch.status {
            finding.status = status;
        }
        if let Some(target) = patch.target {
            finding.target = clip(&target, 500);
        }
        if let Some(evidence) = patch.evidence {
            finding.evidence = Some(clip(&evidence, MAX_EVIDENCE_CHARS));
        }
        if let Some(impact) = patch.impact {
            finding.impact = Some(clip(&impact, MAX_TEXT_CHARS));
        }
        if let Some(remediation) = patch.remediation {
            finding.remediation = Some(clip(&remediation, MAX_TEXT_CHARS));
        }
        if let Some(steps) = patch.poc_steps {
            finding.poc_steps = steps;
        }
        if let Some(confidence) = patch.confidence {
            finding.confidence = Some(confidence);
        }
        finding.confidence = compute_confidence(finding.confidence, &finding.poc_steps);
        finding.updated = Utc::now();
        self.append(&finding)?;
        self.findings[index] = finding.clone();
        Ok(finding)
    }

    pub fn get(&self, id: &str) -> Option<&Finding> {
        self.findings.iter().find(|f| f.id == id)
    }

    /// Persist a replay verdict. Verification never changes the lifecycle
    /// status; `not_reproduced` is not an automatic false positive.
    pub fn record_verification(
        &mut self,
        id: &str,
        verification: Verification,
    ) -> Result<Finding, String> {
        let Some(index) = self.findings.iter().position(|f| f.id == id) else {
            return Err(format!("no finding {id:?}; `finding list` shows ids"));
        };
        let mut finding = self.findings[index].clone();
        finding.verification = Some(verification);
        finding.confidence = compute_confidence(finding.confidence, &finding.poc_steps);
        finding.updated = Utc::now();
        self.append(&finding)?;
        self.findings[index] = finding.clone();
        Ok(finding)
    }

    pub fn counts(&self) -> (usize, usize, usize) {
        let open: Vec<_> = self
            .findings
            .iter()
            .filter(|f| f.status == Status::Open)
            .collect();
        let high = open
            .iter()
            .filter(|f| matches!(f.severity, Severity::Critical | Severity::High))
            .count();
        (open.len(), high, open.len() - high)
    }

    fn append(&self, finding: &Finding) -> Result<(), String> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = self.findings_path();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)
            .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
        let line = serde_json::to_string(finding).map_err(|err| err.to_string())?;
        file.write_all(line.as_bytes())
            .and_then(|_| file.write_all(b"\n"))
            .map_err(|err| format!("cannot write {}: {err}", path.display()))
    }
}

pub fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;

    fn paths(dir: &std::path::Path) -> Paths {
        let mut paths = Paths::discover(dir).unwrap();
        paths.data_dir = dir.join("data");
        paths
    }

    #[test]
    fn findings_append_and_update() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let mut engagement = Engagement::open(&paths, "eng").unwrap();
        let finding = engagement
            .add(NewFinding {
                title: "Reflected XSS".into(),
                severity: Severity::High,
                target: "https://app.test/search".into(),
                evidence: Some("<script>".into()),
                ..NewFinding::default()
            })
            .unwrap();
        assert_eq!(finding.id, "f1");
        assert_eq!(finding.status, Status::Open);
        engagement
            .update(
                "f1",
                FindingPatch {
                    status: Some(Status::Fixed),
                    ..FindingPatch::default()
                },
            )
            .unwrap();
        let reopened = Engagement::open(&paths, "eng").unwrap();
        assert_eq!(reopened.findings.len(), 1);
        assert_eq!(reopened.findings[0].status, Status::Fixed);
    }

    #[test]
    fn caps_are_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let mut engagement = Engagement::open(&paths, "eng").unwrap();
        for i in 0..MAX_FINDINGS {
            engagement
                .add(NewFinding {
                    title: format!("f{i}"),
                    severity: Severity::Low,
                    target: "host".into(),
                    ..NewFinding::default()
                })
                .unwrap();
        }
        assert!(
            engagement
                .add(NewFinding {
                    title: "extra".into(),
                    severity: Severity::Low,
                    target: "host".into(),
                    ..NewFinding::default()
                })
                .is_err()
        );
    }

    #[test]
    fn unknown_id_is_prescriptive() {
        let dir = tempfile::tempdir().unwrap();
        let mut engagement = Engagement::open(&paths(dir.path()), "eng").unwrap();
        assert!(engagement.update("f99", FindingPatch::default()).is_err());
    }

    #[test]
    fn verification_and_confidence_round_trip() {
        use crate::security::verify::{
            PocAction, PocExpect, PocStep, PocStepKind, Verification, VerifyStatus, VerifyVerdict,
        };

        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let mut engagement = Engagement::open(&paths, "eng").unwrap();
        let finding = engagement
            .add(NewFinding {
                title: "proof".into(),
                severity: Severity::High,
                target: "local".into(),
                poc_steps: vec![PocStep {
                    id: "probe".into(),
                    kind: PocStepKind::Verify,
                    summary: "canary".into(),
                    action: PocAction::Shell {
                        cmd: "printf ok".into(),
                        cwd: None,
                    },
                    expect: Some(PocExpect::ExitZero),
                }],
                confidence: Some(0.2),
                ..NewFinding::default()
            })
            .unwrap();
        assert_eq!(finding.confidence, Some(0.8));

        let now = Utc::now();
        engagement
            .record_verification(
                "f1",
                Verification {
                    status: VerifyStatus::Reproduced,
                    verdict: VerifyVerdict::Confirmed,
                    finding_id: "f1".into(),
                    started: now,
                    finished: now,
                    duration_ms: 1,
                    commands: Vec::new(),
                    assertions: Vec::new(),
                    evidence: vec!["e1".into()],
                    summary: "reproduced".into(),
                    error: None,
                },
            )
            .unwrap();

        let reopened = Engagement::open(&paths, "eng").unwrap();
        let finding = reopened.get("f1").unwrap();
        assert_eq!(finding.confidence, Some(0.8));
        assert_eq!(
            finding.verification.as_ref().unwrap().status,
            VerifyStatus::Reproduced
        );
        assert_eq!(finding.verification.as_ref().unwrap().evidence, vec!["e1"]);
        assert_eq!(finding.poc_steps.len(), 1);
    }
}
