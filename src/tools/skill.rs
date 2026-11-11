//! `skill` tool: the model loads a skill's instructions, or one of its
//! references, when the task calls for it.
//!
//! Skills only add instructions. They cannot change scope, tools, gates, or
//! the endpoint; those stay in code. Project skills load only in trusted
//! sessions, exactly as for `/skill`.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::session::{EntryKind, Session};
use crate::skills::{Skill, SkillIndex};
use crate::tools::{Tool, ToolContext, ToolPack, ToolResult, optional_str};

/// Skill text is kept inline up to this size instead of being archived.
pub const SKILL_RESULT_CAP: usize = 24_000;

pub struct SkillTool;

/// The marker a loaded skill starts with, also written by `/skill`.
pub fn skill_marker(name: &str) -> String {
    format!("[skill: {name}]")
}

fn reference_marker(name: &str, path: &str) -> String {
    format!("[skill reference: {name}/{path}]")
}

/// True when `marker` was loaded on the active branch after the most recent
/// compaction, so its text is still in the model's context.
fn loaded_in_context(session: &Session, marker: &str) -> bool {
    let mut loaded = false;
    for entry in session.branch_entries() {
        match &entry.kind {
            EntryKind::Compaction(_) => loaded = false,
            EntryKind::ToolResult(data) if !data.is_error => {
                // Tool results carry a `[tool] skill - …` header line first.
                if data
                    .content
                    .lines()
                    .take(2)
                    .any(|line| line.starts_with(marker))
                {
                    loaded = true;
                }
            }
            EntryKind::User(data) if data.content.starts_with(marker) => loaded = true,
            _ => {}
        }
    }
    loaded
}

fn list(index: &SkillIndex) -> ToolResult {
    if index.skills.is_empty() {
        return ToolResult::ok("no skills are installed");
    }
    let mut out = String::from("available skills (load one with {\"name\":\"<skill>\"}):\n");
    out.push_str(&index.index_text());
    ToolResult::ok(out)
}

fn unknown(index: &SkillIndex, name: &str) -> ToolResult {
    ToolResult::err(
        "unknown_skill",
        format!(
            "no skill named {name:?}; available: {}. Call with no arguments to see descriptions.",
            index.names().join(", ")
        ),
    )
}

fn load_reference(ctx: &ToolContext, skill: &Skill, needle: &str) -> ToolResult {
    let Some(reference) = skill.reference(needle) else {
        let available: Vec<&str> = skill.references.iter().map(|r| r.path.as_str()).collect();
        return ToolResult::err(
            "unknown_reference",
            if available.is_empty() {
                format!("skill {} has no references", skill.name)
            } else {
                format!(
                    "skill {} has no reference {needle:?}; available: {}",
                    skill.name,
                    available.join(", ")
                )
            },
        );
    };
    let marker = reference_marker(&skill.name, &reference.path);
    if already_loaded(ctx, &marker) {
        return ToolResult::ok(format!(
            "{marker} is already loaded earlier in this conversation; use it from there."
        ));
    }
    match reference.text() {
        Some(text) => ToolResult::ok(format!("{marker}\n\n{text}")),
        None => ToolResult::err(
            "unreadable_reference",
            format!("cannot read {} of skill {}", reference.path, skill.name),
        ),
    }
}

fn already_loaded(ctx: &ToolContext, marker: &str) -> bool {
    match ctx.session.lock() {
        Ok(session) => loaded_in_context(&session, marker),
        Err(poison) => loaded_in_context(&poison.into_inner(), marker),
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &'static str {
        "skill"
    }

    fn description(&self) -> &'static str {
        "Load a skill's instructions when the task matches it (see Skills in the system prompt). No arguments lists skills; {\"name\"} loads one; {\"name\",\"reference\"} loads one of its reference documents."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill to load, e.g. security-blue. Omit to list skills."
                },
                "reference": {
                    "type": "string",
                    "description": "Optional reference document of that skill, e.g. references/hardening.md."
                }
            },
            "additionalProperties": false
        })
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Core
    }

    fn read_only(&self) -> bool {
        true
    }

    fn result_cap(&self) -> Option<usize> {
        Some(SKILL_RESULT_CAP)
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let index = SkillIndex::load(&ctx.paths, ctx.trust);
        let name = optional_str(&args, "name")
            .map(|name| name.trim().trim_start_matches('/').to_string())
            .filter(|name| !name.is_empty());
        let Some(name) = name else {
            return list(&index.advertised_for(&ctx.packs));
        };
        let Some(skill) = index.get(&name) else {
            return unknown(&index, &name);
        };
        if let Some(reference) =
            optional_str(&args, "reference").filter(|reference| !reference.trim().is_empty())
        {
            return load_reference(ctx, skill, &reference);
        }
        let marker = skill_marker(&skill.name);
        if already_loaded(ctx, &marker) {
            return ToolResult::ok(format!(
                "{marker} is already loaded earlier in this conversation; follow it from there."
            ));
        }
        ToolResult::ok(format!("{marker}\n\n{}", skill.instructions_text()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionData, ToolResultData, UserData};
    use std::sync::{Arc, Mutex};

    fn context(dir: &std::path::Path, trust: bool) -> ToolContext {
        let mut paths = crate::paths::Paths::discover(dir).unwrap();
        paths.config_dir = dir.join("config");
        paths.data_dir = dir.join("data");
        let session = Session::create(
            &paths,
            SessionData {
                cwd: dir.display().to_string(),
                endpoint: "http://x".into(),
                model: "m".into(),
                profile: "full".into(),
                app: "test".into(),
                redact: true,
                context: None,
            },
            true,
        )
        .unwrap();
        let artifacts = session.artifacts.clone();
        ToolContext {
            cwd: dir.to_path_buf(),
            path_jail: false,
            session: Arc::new(Mutex::new(session)),
            artifacts,
            cancel: Default::default(),
            trust,
            verify: Default::default(),
            profile: "full".into(),
            packs: vec!["core".into(), "coding".into(), "security".into()],
            reads: Default::default(),
            bg: Arc::new(Mutex::new(crate::tools::BgRegistry::new())),
            security: Default::default(),
            paths,
            lsp: Default::default(),
            sandbox: None,
        }
    }

    fn record(ctx: &ToolContext, content: &str) {
        ctx.session
            .lock()
            .unwrap()
            .append(EntryKind::ToolResult(ToolResultData {
                call_id: "c1".into(),
                content: format!("[tool] skill - x\n{content}"),
                is_error: false,
                details: None,
                artifact: None,
            }))
            .unwrap();
    }

    #[tokio::test]
    async fn lists_loads_and_fetches_references() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path(), false);

        let listed = SkillTool.call(&ctx, json!({})).await;
        assert!(
            listed.content.contains("security-red"),
            "{}",
            listed.content
        );

        let loaded = SkillTool.call(&ctx, json!({"name": "security-blue"})).await;
        assert!(!loaded.is_error);
        assert!(loaded.content.starts_with("[skill: security-blue]"));
        assert!(loaded.content.contains("- references/hardening.md"));
        assert!(loaded.content.chars().count() < SKILL_RESULT_CAP);

        let reference = SkillTool
            .call(
                &ctx,
                json!({"name": "security-blue", "reference": "hardening"}),
            )
            .await;
        assert!(!reference.is_error, "{}", reference.content);
        assert!(
            reference
                .content
                .starts_with("[skill reference: security-blue/references/hardening.md]")
        );

        let missing = SkillTool.call(&ctx, json!({"name": "nope"})).await;
        assert!(missing.is_error);
        assert!(missing.content.contains("security-blue"));
        let bad_ref = SkillTool
            .call(&ctx, json!({"name": "security-blue", "reference": "zzz"}))
            .await;
        assert!(bad_ref.is_error);
        assert!(bad_ref.content.contains("references/supply-chain.md"));
    }

    #[tokio::test]
    async fn a_loaded_skill_is_not_repeated_until_compaction_drops_it() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path(), false);
        let first = SkillTool.call(&ctx, json!({"name": "security-red"})).await;
        record(&ctx, &first.content);
        let again = SkillTool.call(&ctx, json!({"name": "security-red"})).await;
        assert!(
            again.content.contains("already loaded"),
            "{}",
            again.content
        );

        ctx.session
            .lock()
            .unwrap()
            .append(EntryKind::Compaction(crate::session::CompactionData {
                reason: "test".into(),
                summary: "s".into(),
                tokens_before: 10,
                thread_id: None,
            }))
            .unwrap();
        let after = SkillTool.call(&ctx, json!({"name": "security-red"})).await;
        assert!(after.content.starts_with("[skill: security-red]"));

        // `/skill` writes the same marker as a user entry.
        ctx.session
            .lock()
            .unwrap()
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
                content: "[skill: security-blue]\n\nbody".into(),
            }))
            .unwrap();
        let blue = SkillTool.call(&ctx, json!({"name": "security-blue"})).await;
        assert!(blue.content.contains("already loaded"));
    }

    #[tokio::test]
    async fn project_skills_need_trust() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join(".harness/skills/deploy");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: deploy\ndescription: Ship it\n---\n\nDeploy steps.\n",
        )
        .unwrap();
        let untrusted = SkillTool
            .call(&context(dir.path(), false), json!({"name": "deploy"}))
            .await;
        assert!(untrusted.is_error);
        let trusted = SkillTool
            .call(&context(dir.path(), true), json!({"name": "deploy"}))
            .await;
        assert!(
            trusted.content.contains("Deploy steps."),
            "{}",
            trusted.content
        );
    }
}
