//! Free helpers for the TUI: endpoint probing, themes, workflows, and
//! session discovery.

use super::*;

pub(super) const LOGIN_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

pub(super) fn normalize_endpoint(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

pub(super) async fn probe_endpoint_models(
    config: &crate::config::ModelConfig,
) -> Result<Vec<String>> {
    let provider = OpenAiCompatProvider::new(config, config.model.clone())?;
    tokio::time::timeout(LOGIN_PROBE_TIMEOUT, probe::list_models(&provider))
        .await
        .map_err(|_| {
            anyhow!(
                "listing models timed out after {}s",
                LOGIN_PROBE_TIMEOUT.as_secs()
            )
        })?
        .map_err(anyhow::Error::from)
}

pub(super) fn login_status(core: &Core) -> String {
    let config = &core.config;
    format!(
        "status: {}\nendpoint: {}\nmodel: {}\napi key: {}\n\nusage:\n  /login <endpoint> [model] [api-key]\n  /login http://127.0.0.1:8080             auto-select one model\n  /login http://127.0.0.1:8080 <model>     choose a model\n  /login <endpoint> <model> -             clear the API key\n\nendpoint, model, and key are saved to {}.",
        if core.logged_in() {
            "logged in"
        } else {
            "not logged in"
        },
        config.model.endpoint,
        if core.logged_in() {
            core.provider.model()
        } else {
            "(not set)"
        },
        if config.model.api_key.is_empty() {
            "not set"
        } else {
            "set"
        },
        core.paths.config_file().display()
    )
}

pub(super) fn estimate_used(core: &Arc<Core>) -> u32 {
    let session = match core.session.lock() {
        Ok(session) => session,
        Err(poison) => poison.into_inner(),
    };
    context::assemble(&core.system_prompt, &session, None, &core.budget).estimated_tokens
}

pub(super) fn state_label(state: Option<AgentState>) -> &'static str {
    match state {
        Some(AgentState::AssembleContext) => "assembling context",
        Some(AgentState::Compact) => "compacting",
        Some(AgentState::StreamModel) => "streaming",
        Some(AgentState::ToolBatch) => "running tools",
        Some(AgentState::PostTool) => "post-tool",
        Some(AgentState::SettleGates) => "settling",
        Some(AgentState::Done) => "done",
        Some(AgentState::Cancelled) => "cancelled",
        Some(AgentState::Error) => "error",
        None => "idle",
    }
}

pub(super) fn short_session_id(id: &str) -> String {
    if id == "ephemeral" {
        return id.to_string();
    }
    let chars: Vec<char> = id.chars().collect();
    if chars.len() <= 8 {
        return id.to_string();
    }
    chars[chars.len() - 8..].iter().collect()
}

/// Persist the applied theme to the global config, preserving unrelated keys
/// and the file mode. `Config::save_ui_setting` does the atomic write.
pub(super) fn save_theme(path: &Path, theme: &str) -> Result<()> {
    Config::save_ui_setting(path, "theme", toml::Value::String(theme.to_string()))
}

pub(super) fn available_theme_names(paths: &crate::paths::Paths) -> Vec<String> {
    let mut names: Vec<String> = Theme::names().iter().map(|name| name.to_string()).collect();
    if let Ok(entries) = std::fs::read_dir(paths.themes_dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("toml")
                && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
                && !names.iter().any(|name| name == stem)
            {
                names.push(stem.to_string());
            }
        }
    }
    names.sort();
    names
}

/// Load an installable workflow TOML and its prompt, resolved relative to the
/// file. Errors are the prescriptive messages `/workflow` has always shown.
pub(super) fn load_workflow_file(path: &Path) -> std::result::Result<WorkflowSpec, String> {
    crate::workflows::load_file(path)
}

/// Picker rows for the endpoint's models.
pub(super) fn model_items(models: &[String]) -> Vec<PickItem> {
    models
        .iter()
        .map(|model| PickItem::run("model", model.clone(), ""))
        .collect()
}

/// Skills the active tools advertise, with their descriptions.
pub(super) fn skill_choices(
    paths: &crate::paths::Paths,
    trust: bool,
    tools: &[Arc<dyn crate::tools::Tool>],
) -> Vec<(String, String)> {
    let index = SkillIndex::load(paths, trust);
    let advertised = index.advertised_for(&crate::tools::active_packs(tools));
    advertised
        .names()
        .into_iter()
        .map(|name| {
            let description = advertised
                .get(name)
                .map(|skill| skill.description.clone())
                .unwrap_or_default();
            (name.to_string(), description)
        })
        .collect()
}

/// One-line description of a bundled workflow (file workflows are only read
/// when run).
pub(super) fn workflow_description(name: &str) -> String {
    workflows::get(name)
        .and_then(|workflow| workflow.description.clone())
        .unwrap_or_default()
}

pub(super) fn available_workflows(paths: &crate::paths::Paths, trust: bool) -> Vec<String> {
    let mut names = Vec::new();
    let mut scan = |dir: &Path| {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) == Some("toml")
                    && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
                    && !names.iter().any(|name| name == stem)
                {
                    names.push(stem.to_string());
                }
            }
        }
    };
    scan(&paths.workflows_dir());
    if trust {
        scan(&paths.project_dir().join("workflows"));
    }
    for name in workflows::names() {
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_string());
        }
    }
    names.sort();
    names
}

pub(super) fn scan_sessions(paths: &crate::paths::Paths) -> Vec<SessionItem> {
    crate::session::list_session_files(&paths.project_sessions_dir())
        .into_iter()
        .map(|session| SessionItem {
            id: session.id,
            preview: clip_end(&first_line(&session.first_user), 80),
            when: relative_when(
                chrono::DateTime::<chrono::Local>::from(session.modified),
                chrono::Local::now(),
            ),
            path: session.path,
        })
        .collect()
}

/// How long ago a session was last touched, the way a person says it; older
/// than a week shows the date.
pub(super) fn relative_when(
    then: chrono::DateTime<chrono::Local>,
    now: chrono::DateTime<chrono::Local>,
) -> String {
    let minutes = (now - then).num_minutes().max(0);
    let days = (now.date_naive() - then.date_naive()).num_days();
    match minutes {
        0 => "just now".into(),
        1..=59 => format!("{minutes} min ago"),
        _ if days == 0 => format!("{} h ago", minutes / 60),
        _ if days == 1 => format!("yesterday {}", then.format("%H:%M")),
        _ if days < 7 => format!("{days} days ago"),
        _ => then.format("%Y-%m-%d").to_string(),
    }
}

/// The first user message as one line, for a picker row.
fn first_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Resolve `/resume <id>`: an exact id, else a unique id prefix or suffix (the
/// header shows the last 8 characters).
pub(super) fn match_session(items: &[SessionItem], needle: &str) -> Result<PathBuf, String> {
    if let Some(item) = items.iter().find(|item| item.id == needle) {
        return Ok(item.path.clone());
    }
    let matches: Vec<&SessionItem> = items
        .iter()
        .filter(|item| item.id.starts_with(needle) || item.id.ends_with(needle))
        .collect();
    match matches.as_slice() {
        [only] => Ok(only.path.clone()),
        [] => Err(format!(
            "no session id matches {needle:?}.\nRun /resume to pick from a list, or /resume last."
        )),
        many => Err(format!(
            "{needle:?} matches {} sessions:\n{}\n\nUse a longer id.",
            many.len(),
            many.iter()
                .map(|item| format!("  {}  {}  {}", item.id, item.when, item.preview))
                .collect::<Vec<_>>()
                .join("\n")
        )),
    }
}

#[cfg(test)]
mod theme_tests {
    use super::save_theme;

    #[test]
    fn save_theme_preserves_the_file_mode_and_unrelated_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "default_profile = \"full\"\n\n[model]\napi_key = \"secret\"\n\n[ui]\ntheme = \"light\"\ncolor_mode = \"256\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        save_theme(&path, "hati").unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Value = toml::from_str(&raw).unwrap();
        assert_eq!(parsed["ui"]["theme"].as_str(), Some("hati"));
        assert_eq!(parsed["ui"]["color_mode"].as_str(), Some("256"));
        assert_eq!(parsed["model"]["api_key"].as_str(), Some("secret"));
        assert_eq!(parsed["default_profile"].as_str(), Some("full"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the private config stays private");
        }
    }
}

#[cfg(test)]
mod when_tests {
    use super::relative_when;
    use chrono::{Duration, Local, TimeZone};

    #[test]
    fn session_times_read_like_speech() {
        let now = Local.with_ymd_and_hms(2026, 11, 17, 18, 0, 0).unwrap();
        let ago = |delta: Duration| relative_when(now - delta, now);
        assert_eq!(ago(Duration::seconds(20)), "just now");
        assert_eq!(ago(Duration::minutes(5)), "5 min ago");
        assert_eq!(ago(Duration::hours(3)), "3 h ago");
        assert_eq!(ago(Duration::hours(20)), "yesterday 22:00");
        assert_eq!(ago(Duration::days(4)), "4 days ago");
        assert_eq!(ago(Duration::days(30)), "2026-10-18");
    }
}
