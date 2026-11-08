//! Markdown engagement report.

use super::findings::{Engagement, Severity, Status};

/// Render findings grouped by severity, most severe first.
pub fn render(engagement: &Engagement) -> String {
    let mut out = format!("# Findings — {}\n\n", engagement.name);
    let (open, high, medium) = engagement.counts();
    out.push_str(&format!(
        "{} findings, {} open ({} high/critical, {} medium/low)\n\n",
        engagement.findings.len(),
        open,
        high,
        medium
    ));
    if engagement.findings.is_empty() {
        out.push_str("No findings recorded.\n");
        return out;
    }
    for severity in [
        Severity::Critical,
        Severity::High,
        Severity::Medium,
        Severity::Low,
        Severity::Info,
    ] {
        let group: Vec<_> = engagement
            .findings
            .iter()
            .filter(|f| f.severity == severity)
            .collect();
        if group.is_empty() {
            continue;
        }
        out.push_str(&format!("## {}\n\n", title_case(severity.as_str())));
        for finding in group {
            out.push_str(&format!(
                "### {} — {} ({})\n\n",
                finding.id, finding.title, finding.status
            ));
            out.push_str(&format!("- Target: `{}`\n", finding.target));
            if let Some(evidence) = &finding.evidence {
                out.push_str(&format!("- Evidence: {evidence}\n"));
            }
            if let Some(reference) = &finding.evidence_ref {
                let mut parts = Vec::new();
                if let Some(session) = &reference.session {
                    parts.push(format!("session `{session}`"));
                }
                if let Some(entry) = &reference.entry {
                    parts.push(format!("entry `{entry}`"));
                }
                if let Some(artifact) = &reference.artifact {
                    parts.push(format!("artifact `{artifact}`"));
                }
                if !parts.is_empty() {
                    out.push_str(&format!("- Evidence ref: {}\n", parts.join(", ")));
                }
            }
            if let Some(impact) = &finding.impact {
                out.push_str(&format!("- Impact: {impact}\n"));
            }
            if let Some(remediation) = &finding.remediation {
                out.push_str(&format!("- Remediation: {remediation}\n"));
            }
            if finding.status == Status::Open {
                out.push_str("- Status: open\n");
            }
            out.push('\n');
        }
    }
    out
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::findings::NewFinding;

    #[test]
    fn report_groups_by_severity() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = crate::paths::Paths::discover(dir.path()).unwrap();
        paths.data_dir = dir.path().join("data");
        let mut engagement = Engagement::open(&paths, "eng").unwrap();
        engagement
            .add(NewFinding {
                title: "SQLi".into(),
                severity: Severity::Critical,
                target: "db.test".into(),
                ..NewFinding::default()
            })
            .unwrap();
        engagement
            .add(NewFinding {
                title: "Banner".into(),
                severity: Severity::Info,
                target: "web.test".into(),
                ..NewFinding::default()
            })
            .unwrap();
        let report = render(&engagement);
        assert!(report.find("## Critical").unwrap() < report.find("## Info").unwrap());
        assert!(report.contains("db.test"));
    }
}
