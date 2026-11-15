//! `lh skill eval`: trigger precision and recall over prompt fixtures.
//!
//! The CI provider is deterministic: it scores the prompt against the skill
//! name, description, and `when` hint, loads the best match, and picks a
//! reference by name and content overlap. No model, no network. A fixture that
//! says `expect = "skip"` and loads is a failure, not a warning.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::paths::Paths;

use super::{Skill, SkillIndex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    Load,
    Skip,
}

impl Expect {
    pub fn label(self) -> &'static str {
        match self {
            Expect::Load => "load",
            Expect::Skip => "skip",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Fixture {
    pub name: String,
    pub prompt: String,
    pub skill: String,
    pub expect: Expect,
    pub reference: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureFile {
    prompt: String,
    expect: String,
    skill: Option<String>,
    reference: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FixtureResult {
    pub name: String,
    pub prompt: String,
    pub skill: String,
    pub expect: Expect,
    pub predicted: Option<String>,
    pub reference_expected: Option<String>,
    pub reference_loaded: Option<String>,
    pub loaded_tokens: usize,
    pub passed: bool,
    pub failure: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SkillMetrics {
    pub skill: String,
    pub true_positives: usize,
    pub false_positives: usize,
    pub false_negatives: usize,
    pub true_negatives: usize,
    pub precision: Option<f64>,
    pub recall: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct EvalReport {
    pub provider: String,
    pub fixtures: Vec<FixtureResult>,
    pub skills: Vec<SkillMetrics>,
    pub average_loaded_tokens: f64,
    pub false_load_cost: usize,
    pub failures: usize,
    pub passed: bool,
}

impl EvalReport {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "provider": self.provider,
            "passed": self.passed,
            "failures": self.failures,
            "average_loaded_tokens": self.average_loaded_tokens,
            "false_load_cost": self.false_load_cost,
            "skills": self.skills.iter().map(|metrics| serde_json::json!({
                "skill": metrics.skill,
                "precision": metrics.precision,
                "recall": metrics.recall,
                "true_positives": metrics.true_positives,
                "false_positives": metrics.false_positives,
                "false_negatives": metrics.false_negatives,
                "true_negatives": metrics.true_negatives,
            })).collect::<Vec<_>>(),
            "fixtures": self.fixtures.iter().map(|fixture| serde_json::json!({
                "name": fixture.name,
                "prompt": fixture.prompt,
                "skill": fixture.skill,
                "expect": fixture.expect.label(),
                "predicted": fixture.predicted,
                "reference_expected": fixture.reference_expected,
                "reference_loaded": fixture.reference_loaded,
                "loaded_tokens": fixture.loaded_tokens,
                "passed": fixture.passed,
                "failure": fixture.failure,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Run the deterministic provider over every fixture in `dir`.
pub fn run(paths: &Paths, trust: bool, dir: &Path) -> Result<EvalReport, String> {
    let fixtures = load_fixtures(dir)?;
    let index = SkillIndex::load(paths, trust);
    let mut results = Vec::new();
    for fixture in fixtures {
        let (predicted, reference, tokens, failure) = evaluate_one(&index, &fixture);
        let loaded_tokens = tokens;
        let passed = failure.is_none();
        results.push(FixtureResult {
            name: fixture.name.clone(),
            prompt: fixture.prompt.clone(),
            skill: fixture.skill.clone(),
            expect: fixture.expect,
            predicted,
            reference_expected: fixture.reference.clone(),
            reference_loaded: reference,
            loaded_tokens,
            passed,
            failure,
        });
    }
    let failures = results.iter().filter(|result| !result.passed).count();
    let total_tokens: usize = results.iter().map(|result| result.loaded_tokens).sum();
    let average_loaded_tokens = if results.is_empty() {
        0.0
    } else {
        total_tokens as f64 / results.len() as f64
    };
    let false_load_cost: usize = results
        .iter()
        .filter(|result| result.expect == Expect::Skip)
        .map(|result| result.loaded_tokens)
        .sum();
    let skills = metrics(&results);
    Ok(EvalReport {
        provider: "fake".into(),
        passed: failures == 0,
        failures,
        average_loaded_tokens,
        false_load_cost,
        fixtures: results,
        skills,
    })
}

/// Estimate tokens the way the harness budget does for prose.
pub fn estimate_tokens(text: &str) -> usize {
    ((text.chars().count() as f64) / crate::context::budget::DEFAULT_CHARS_PER_TOKEN).ceil()
        as usize
}

fn evaluate_one(
    index: &SkillIndex,
    fixture: &Fixture,
) -> (Option<String>, Option<String>, usize, Option<String>) {
    let target = index.get(&fixture.skill);
    if target.is_none() {
        return (
            None,
            None,
            fixture.reference.as_ref().map(|_| 0).unwrap_or(0),
            Some(format!(
                "fixture `{}` names unknown skill `{}`",
                fixture.name, fixture.skill
            )),
        );
    }
    let (predicted, _score) = best_match(index, &fixture.prompt);
    let predicted_skill = predicted.as_ref().and_then(|name| index.get(name));
    let reference = predicted_skill.and_then(|skill| {
        choose_reference(skill, &fixture.prompt).map(|reference| reference.path.clone())
    });
    let mut tokens = predicted_skill
        .map(|skill| estimate_tokens(&skill.instructions_text()))
        .unwrap_or(0);
    if let Some(skill) = predicted_skill
        && let Some(path) = &reference
        && let Some(reference) = skill.reference(path)
    {
        tokens += reference
            .text()
            .map(|text| estimate_tokens(&text))
            .unwrap_or(0);
    }

    let mut failure = None;
    match fixture.expect {
        Expect::Load => {
            if predicted.as_deref() != Some(fixture.skill.as_str()) {
                failure = Some(match &predicted {
                    Some(other) => format!(
                        "expected `{}` to load, but `{other}` loaded instead",
                        fixture.skill
                    ),
                    None => format!("expected `{}` to load, but nothing loaded", fixture.skill),
                });
            } else if let Some(expected) = &fixture.reference
                && !same_reference(reference.as_deref(), expected)
            {
                failure = Some(match &reference {
                    Some(loaded) => {
                        format!("expected reference `{expected}`, but `{loaded}` loaded")
                    }
                    None => format!("expected reference `{expected}`, but none loaded"),
                });
            }
        }
        Expect::Skip => {
            if let Some(loaded) = &predicted {
                failure = Some(format!(
                    "expected `{}` to skip, but `{loaded}` loaded (false-load cost {tokens} tokens)",
                    fixture.skill
                ));
            }
        }
    }
    (predicted, reference, tokens, failure)
}

fn same_reference(loaded: Option<&str>, expected: &str) -> bool {
    loaded.is_some_and(|loaded| {
        loaded == expected
            || loaded.ends_with(expected)
            || expected.ends_with(loaded)
            || loaded
                .rsplit('/')
                .next()
                .is_some_and(|file| expected.rsplit('/').next() == Some(file))
    })
}

/// The fake provider: name terms weigh more than description terms, and a
/// single strong name match is enough to load.
fn best_match(index: &SkillIndex, prompt: &str) -> (Option<String>, f64) {
    let prompt_terms = terms(prompt);
    if prompt_terms.is_empty() {
        return (None, 0.0);
    }
    let mut best: Option<(String, f64)> = None;
    for skill in &index.skills {
        let name_terms = terms(&skill.name.replace('-', " "));
        let hint = format!(
            "{} {}",
            skill.description,
            skill.when.as_deref().unwrap_or_default()
        );
        let hint_terms = terms(&hint);
        let mut weight = 0.0;
        for term in &prompt_terms {
            if name_terms.contains(term) {
                weight += 3.0;
            } else if hint_terms.contains(term) {
                weight += 1.0;
            }
        }
        let mut score = weight / prompt_terms.len() as f64;
        if prompt
            .to_ascii_lowercase()
            .contains(&skill.name.replace('-', " "))
        {
            score += 0.5;
        }
        if score > best.as_ref().map(|(_, score)| *score).unwrap_or(0.0) {
            best = Some((skill.name.clone(), score));
        }
    }
    match best {
        Some((name, score)) if score >= 0.5 => (Some(name), score),
        _ => (None, 0.0),
    }
}

fn choose_reference(skill: &Skill, prompt: &str) -> Option<super::SkillReference> {
    let prompt_terms = terms(prompt);
    let mut best: Option<(super::SkillReference, f64)> = None;
    for reference in skill.references() {
        let path_terms = terms(&reference.path.replace(['/', '.', '-', '_'], " "));
        let mut score = path_terms
            .iter()
            .filter(|term| prompt_terms.contains(term))
            .count() as f64
            * 4.0;
        if let Some(text) = reference.text() {
            let content_terms = terms(&text);
            score += content_terms
                .iter()
                .filter(|term| prompt_terms.contains(term))
                .count() as f64
                * 0.2;
        }
        if score > best.as_ref().map(|(_, score)| *score).unwrap_or(0.0) {
            best = Some((reference, score));
        }
    }
    best.map(|(reference, _)| reference)
}

fn terms(text: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "the", "and", "for", "with", "that", "this", "from", "when", "use", "into", "your", "you",
        "are", "was", "were", "has", "have", "had", "not", "but", "all", "any", "can", "will",
        "would", "should", "could", "its", "our", "their", "them", "they", "then", "than", "there",
        "here", "about", "after", "before", "over", "under", "only", "also", "may", "might",
        "must", "does", "did", "done", "being", "been", "one", "two", "out", "off", "per", "via",
        "how", "why", "what", "which", "who", "whom", "whose", "each", "more", "most", "some",
        "such", "very", "just", "like", "make", "made", "get", "set",
    ];
    let mut out = Vec::new();
    for word in text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .map(|word| word.to_ascii_lowercase())
        .filter(|word| word.len() >= 3 && !STOP.contains(&word.as_str()))
    {
        if !out.iter().any(|existing| existing == &word) {
            out.push(word);
        }
    }
    out
}

fn metrics(results: &[FixtureResult]) -> Vec<SkillMetrics> {
    let mut skills: Vec<String> = results.iter().map(|result| result.skill.clone()).collect();
    skills.sort();
    skills.dedup();
    skills
        .into_iter()
        .map(|skill| {
            let mut true_positives = 0;
            let mut false_positives = 0;
            let mut false_negatives = 0;
            let mut true_negatives = 0;
            for result in results.iter().filter(|result| result.skill == skill) {
                match (
                    result.expect,
                    result.predicted.as_deref() == Some(skill.as_str()),
                ) {
                    (Expect::Load, true) => true_positives += 1,
                    (Expect::Load, false) => false_negatives += 1,
                    (Expect::Skip, true) => false_positives += 1,
                    (Expect::Skip, false) => true_negatives += 1,
                }
            }
            let precision = if true_positives + false_positives == 0 {
                None
            } else {
                Some(true_positives as f64 / (true_positives + false_positives) as f64)
            };
            let recall = if true_positives + false_negatives == 0 {
                None
            } else {
                Some(true_positives as f64 / (true_positives + false_negatives) as f64)
            };
            SkillMetrics {
                skill,
                true_positives,
                false_positives,
                false_negatives,
                true_negatives,
                precision,
                recall,
            }
        })
        .collect()
}

fn load_fixtures(dir: &Path) -> Result<Vec<Fixture>, String> {
    if !dir.is_dir() {
        return Err(format!(
            "--fixtures {} is not a directory; pass a directory of .toml or .json fixtures",
            dir.display()
        ));
    }
    let entries = std::fs::read_dir(dir)
        .map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension == "toml" || extension == "json")
        })
        .collect();
    paths.sort();
    if paths.is_empty() {
        return Err(format!(
            "no fixtures in {}; add a .toml file with prompt, skill, and expect = \"load\"|\"skip\"",
            dir.display()
        ));
    }
    let mut fixtures = Vec::new();
    for path in paths {
        let raw = std::fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        let file: FixtureFile = if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            serde_json::from_str(&raw)
                .map_err(|error| format!("{} is not a valid fixture: {error}", path.display()))?
        } else {
            toml::from_str(&raw)
                .map_err(|error| format!("{} is not a valid fixture: {error}", path.display()))?
        };
        let expect = match file.expect.trim().to_ascii_lowercase().as_str() {
            "load" => Expect::Load,
            "skip" => Expect::Skip,
            other => {
                return Err(format!(
                    "{} has expect = {other:?}; use \"load\" or \"skip\"",
                    path.display()
                ));
            }
        };
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("fixture")
            .to_string();
        let Some(skill) = file.skill else {
            return Err(format!(
                "{} must name the skill under test with `skill = \"<name>\"`",
                path.display()
            ));
        };
        fixtures.push(Fixture {
            name,
            prompt: file.prompt,
            skill,
            expect,
            reference: file.reference,
        });
    }
    Ok(fixtures)
}

/// Convenience for tests and a future `--json`-free summary.
pub fn metrics_by_skill(report: &EvalReport) -> BTreeMap<String, SkillMetrics> {
    report
        .skills
        .iter()
        .cloned()
        .map(|metrics| (metrics.skill.clone(), metrics))
        .collect()
}
