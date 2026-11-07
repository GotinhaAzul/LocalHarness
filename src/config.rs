//! Configuration: TOML loading, layered precedence, model profiles, verify steps.
//!
//! Precedence: CLI flags > project config (when trusted) > global config >
//! built-in defaults. Environment variables sit between project and global
//! where noted.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const DEFAULT_CONTEXT_WINDOW: u32 = 32_768;
pub const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8080";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub model: ModelConfig,
    pub ui: UiConfig,
    pub session: SessionConfig,
    pub budgets: BudgetConfig,
    #[serde(rename = "profile")]
    pub profiles: BTreeMap<String, ProfileConfig>,
    #[serde(rename = "model_profile")]
    pub model_profiles: BTreeMap<String, ModelProfile>,
    pub verify: VerifyConfig,
    /// Runtime selection, set from CLI or environment.
    #[serde(skip)]
    pub profile_name: String,
    #[serde(skip)]
    pub model_profile_override: Option<String>,
    #[serde(skip)]
    pub trust: bool,
}

impl Config {
    /// Built-in defaults, including the `full` profile and the default model
    /// profile.
    pub fn builtin() -> Self {
        let mut cfg = Config::default();
        cfg.profiles.insert(
            "full".into(),
            ProfileConfig {
                packs: vec!["core".into(), "coding".into(), "security".into()],
                tools: vec![],
                model_profile: Some("auto".into()),
                prompt: None,
                description: Some("Coding and security, all built-in packs".into()),
                theme: None,
            },
        );
        cfg.profiles.insert(
            "coding".into(),
            ProfileConfig {
                packs: vec!["core".into(), "coding".into()],
                tools: vec![],
                model_profile: None,
                prompt: None,
                description: Some("Coding only".into()),
                theme: None,
            },
        );
        cfg.profiles.insert(
            "security".into(),
            ProfileConfig {
                packs: vec!["core".into(), "security".into()],
                tools: vec![],
                model_profile: None,
                prompt: None,
                description: Some("Security only".into()),
                theme: None,
            },
        );
        cfg.profiles.insert(
            "minimal".into(),
            ProfileConfig {
                packs: vec![],
                tools: vec!["read".into(), "bash".into(), "verify".into()],
                model_profile: None,
                prompt: None,
                description: Some("read, bash, verify".into()),
                theme: None,
            },
        );
        cfg.model_profiles.insert(
            "default".into(),
            ModelProfile {
                context_window: Some(DEFAULT_CONTEXT_WINDOW),
                tool_mode: ToolMode::Repair,
                strip_reasoning: Some(true),
                ..ModelProfile::default()
            },
        );
        cfg
    }

    /// Load defaults, then the global config, then an optional trusted project
    /// config. `project` is only passed when trust is granted.
    pub fn load(global: &Path, project: Option<&Path>) -> Result<Self> {
        let mut value = toml::Value::try_from(Self::builtin()).context("serialize defaults")?;
        if global.exists() {
            let raw = read_toml(global)?;
            deep_merge(&mut value, raw);
        }
        if let Some(project) = project {
            if project.exists() {
                let raw = read_toml(project)?;
                deep_merge(&mut value, raw);
            }
        }
        let mut cfg: Config = value.try_into().context("config schema mismatch")?;
        cfg.apply_env();
        cfg.validate()?;
        Ok(cfg)
    }

    /// Environment overrides. `LH_ENDPOINT`, `LH_MODEL`, `LH_API_KEY`,
    /// `LH_PROFILE`, `LH_MODEL_PROFILE`, `LH_THEME` are read.
    pub fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("LH_ENDPOINT") {
            if !v.trim().is_empty() {
                self.model.endpoint = v;
            }
        }
        if let Ok(v) = std::env::var("LH_MODEL") {
            self.model.model = v;
        }
        if let Ok(v) = std::env::var("LH_API_KEY") {
            self.model.api_key = v;
        }
        if let Ok(v) = std::env::var("LH_PROFILE") {
            if !v.trim().is_empty() {
                self.profile_name = v;
            }
        }
        if let Ok(v) = std::env::var("LH_MODEL_PROFILE") {
            if !v.trim().is_empty() {
                self.model_profile_override = Some(v);
            }
        }
        if let Ok(v) = std::env::var("LH_THEME") {
            if !v.trim().is_empty() {
                self.ui.theme = v;
            }
        }
        if std::env::var("LH_NO_SESSION").is_ok() {
            self.session.no_session = true;
        }
        if std::env::var("LH_TRUST").is_ok() {
            self.trust = true;
        }
    }

    fn validate(&self) -> Result<()> {
        if self.budgets.max_model_calls == 0 {
            bail!("budgets.max_model_calls must be at least 1");
        }
        if self.budgets.max_tool_calls == 0 {
            bail!("budgets.max_tool_calls must be at least 1");
        }
        if self.model.context_window == Some(0) {
            bail!("model.context_window must be greater than 0");
        }
        if self.model.max_output_tokens == 0 {
            bail!("model.max_output_tokens must be greater than 0");
        }
        Ok(())
    }

    /// The active profile name. `full` unless configured otherwise.
    pub fn profile_name(&self) -> &str {
        if self.profile_name.is_empty() {
            "full"
        } else {
            &self.profile_name
        }
    }

    pub fn profile(&self) -> &ProfileConfig {
        static FALLBACK: std::sync::LazyLock<ProfileConfig> = std::sync::LazyLock::new(|| {
            ProfileConfig {
                packs: vec!["core".into(), "coding".into(), "security".into()],
                tools: vec![],
                model_profile: Some("auto".into()),
                prompt: None,
                description: None,
                theme: None,
            }
        });
        self.profiles
            .get(self.profile_name())
            .unwrap_or(&FALLBACK)
    }

    /// Resolve the model profile for `model`. `auto` matches by prefix declared
    /// in `match`; otherwise `default`.
    pub fn model_profile(&self) -> (String, ModelProfile) {
        let selected = self
            .model_profile_override
            .clone()
            .or_else(|| self.profile().model_profile.clone())
            .unwrap_or_else(|| "auto".into());
        if selected != "auto" {
            if let Some(profile) = self.model_profiles.get(&selected) {
                return (selected, profile.clone());
            }
            let default = self
                .model_profiles
                .get("default")
                .cloned()
                .unwrap_or_default();
            return (selected, default);
        }
        let model = self.model.model.to_ascii_lowercase();
        for (name, profile) in &self.model_profiles {
            if name == "default" {
                continue;
            }
            if profile
                .match_
                .iter()
                .any(|m| !m.is_empty() && model.starts_with(&m.to_ascii_lowercase()))
            {
                return (name.clone(), profile.clone());
            }
        }
        let default = self
            .model_profiles
            .get("default")
            .cloned()
            .unwrap_or_default();
        ("default".into(), default)
    }

    /// Context window: model profile, then explicit `[model]` value, then
    /// default.
    pub fn context_window(&self) -> u32 {
        let (_, profile) = self.model_profile();
        profile
            .context_window
            .or(self.model.context_window)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW)
    }

    pub fn max_output_tokens(&self) -> u32 {
        self.model.max_output_tokens
    }

    /// Load a profile prompt file (project-relative when inside `.harness`).
    pub fn load_profile_prompt(&self, paths: &crate::paths::Paths) -> Option<String> {
        let rel = self.profile().prompt.as_ref()?;
        let candidates = [
            paths.project_dir().join(rel),
            paths.config_dir.join(rel),
            PathBuf::from(rel),
        ];
        candidates
            .iter()
            .find(|p| p.is_file())
            .and_then(|p| std::fs::read_to_string(p).ok())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    /// `None` means "not explicitly configured".
    pub context_window: Option<u32>,
    pub temperature: f32,
    pub top_p: f32,
    pub max_output_tokens: u32,
    pub request_timeout_secs: u64,
    pub connect_timeout_secs: u64,
    pub first_token_timeout_secs: u64,
    pub stream_idle_timeout_secs: u64,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.into(),
            model: String::new(),
            api_key: String::new(),
            context_window: None,
            temperature: 0.2,
            top_p: 0.95,
            max_output_tokens: 4096,
            request_timeout_secs: 900,
            connect_timeout_secs: 5,
            first_token_timeout_secs: 120,
            stream_idle_timeout_secs: 120,
        }
    }
}

impl ModelConfig {
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(self.connect_timeout_secs.max(1))
    }
    pub fn total_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout_secs.max(1))
    }
    pub fn first_token_timeout(&self) -> Duration {
        Duration::from_secs(self.first_token_timeout_secs.max(1))
    }
    pub fn stream_idle_timeout(&self) -> Duration {
        Duration::from_secs(self.stream_idle_timeout_secs.max(1))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub theme: String,
    pub color_mode: ColorMode,
    pub show_reasoning: bool,
    pub show_token_counts: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "omarchy".into(),
            color_mode: ColorMode::Auto,
            show_reasoning: false,
            show_token_counts: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ColorMode {
    #[default]
    Auto,
    Truecolor,
    #[serde(rename = "256")]
    Ansi256,
    #[serde(rename = "16")]
    Ansi16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Empty means the default data directory.
    pub dir: PathBuf,
    pub no_session: bool,
    /// Artifact retention in days; 0 disables cleanup.
    pub artifact_retention_days: u32,
    pub keep_artifacts: bool,
    pub clean_artifacts: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::new(),
            no_session: false,
            artifact_retention_days: 30,
            keep_artifacts: false,
            clean_artifacts: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetConfig {
    pub max_tool_calls: u32,
    pub max_model_calls: u32,
    pub max_wall_minutes: u32,
    pub stall_calls: u32,
    pub verify_attempts: u32,
    pub wrap_up_at: f32,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            max_tool_calls: 120,
            max_model_calls: 40,
            max_wall_minutes: 30,
            stall_calls: 12,
            verify_attempts: 2,
            wrap_up_at: 0.8,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileConfig {
    pub packs: Vec<String>,
    pub tools: Vec<String>,
    pub model_profile: Option<String>,
    pub prompt: Option<PathBuf>,
    pub description: Option<String>,
    pub theme: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelProfile {
    #[serde(rename = "match")]
    pub match_: Vec<String>,
    pub context_window: Option<u32>,
    pub strip_reasoning: Option<bool>,
    pub reasoning_field: Option<String>,
    pub tool_mode: ToolMode,
    pub parallel_tool_calls: Option<bool>,
    pub stop: Vec<String>,
    pub extra_body: Option<toml::Value>,
    /// Hint for tool schema style; the harness always sends JSON schema.
    pub native_tools: Option<bool>,
}

impl Default for ModelProfile {
    fn default() -> Self {
        Self {
            match_: vec![],
            context_window: None,
            strip_reasoning: Some(true),
            reasoning_field: None,
            tool_mode: ToolMode::Auto,
            parallel_tool_calls: None,
            stop: vec![],
            extra_body: None,
            native_tools: None,
        }
    }
}

impl ModelProfile {
    pub fn strip_reasoning(&self) -> bool {
        self.strip_reasoning.unwrap_or(true)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolMode {
    Native,
    Repair,
    #[default]
    Auto,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifyConfig {
    pub steps: Vec<VerifyStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyStep {
    pub name: String,
    /// Program plus arguments; never run through a shell.
    pub command: Vec<String>,
    #[serde(default = "default_step_timeout")]
    pub timeout_secs: u64,
}

fn default_step_timeout() -> u64 {
    600
}

impl VerifyConfig {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let cfg: VerifyConfig = toml::from_str(&raw)
            .with_context(|| format!("invalid verify config {}", path.display()))?;
        for step in &cfg.steps {
            if step.command.is_empty() {
                bail!("verify step {:?} has an empty command", step.name);
            }
        }
        Ok(cfg)
    }
}

fn read_toml(path: &Path) -> Result<toml::Value> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let value: toml::Value = toml::from_str(&raw)
        .with_context(|| format!("invalid TOML in {}", path.display()))?;
    Ok(value)
}

/// Recursively overlay `overlay` onto `base`. Tables merge; everything else
/// replaces.
pub fn deep_merge(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(slot) => deep_merge(slot, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (slot, value) => *slot = value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = Config::builtin();
        assert_eq!(cfg.profile_name(), "full");
        assert_eq!(cfg.context_window(), 32768);
        assert_eq!(cfg.profile().packs, ["core", "coding", "security"]);
    }

    #[test]
    fn auto_profile_matches_prefix() {
        let mut cfg = Config::builtin();
        cfg.model.model = "qwen3.5-35b-a3b".into();
        cfg.model_profiles.insert(
            "qwen3".into(),
            ModelProfile {
                match_: vec!["qwen3".into()],
                context_window: Some(65_536),
                strip_reasoning: Some(true),
                tool_mode: ToolMode::Native,
                ..ModelProfile::default()
            },
        );
        let (name, profile) = cfg.model_profile();
        assert_eq!(name, "qwen3");
        assert_eq!(cfg.context_window(), 65_536);
        assert!(profile.strip_reasoning());
    }

    #[test]
    fn deep_merge_replaces_scalars_and_merges_tables() {
        let mut base: toml::Value = toml::from_str("[a]\nx=1\ny=2\n[b]\nz=3\n").unwrap();
        let over: toml::Value = toml::from_str("[a]\nx=9\n[c]\nq=4\n").unwrap();
        deep_merge(&mut base, over);
        assert_eq!(base["a"]["x"].as_integer(), Some(9));
        assert_eq!(base["a"]["y"].as_integer(), Some(2));
        assert_eq!(base["c"]["q"].as_integer(), Some(4));
        assert_eq!(base["b"]["z"].as_integer(), Some(3));
    }
}
