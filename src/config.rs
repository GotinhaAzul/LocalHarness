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
    pub prompt: PromptConfig,
    pub lsp: LspConfig,
    /// Profile used when neither `--profile` nor `LH_PROFILE` is set.
    pub default_profile: String,
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
    /// OS-level sandbox for `bash` and `bg`; runtime only.
    #[serde(skip)]
    pub sandbox: SandboxMode,
    /// True when the user passed `--sandbox` or `--no-sandbox`.
    #[serde(skip)]
    pub sandbox_explicit: bool,
}

/// How strictly `bash` and `bg` are isolated from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxMode {
    /// Run unsandboxed, exactly as before the sandbox existed.
    #[default]
    Off,
    /// The whole filesystem is read-only; only the private temp directory is
    /// writable.
    ReadOnly,
    /// The workspace is writable; everything else stays read-only. An existing
    /// `.git` inside the workspace is mounted read-only.
    WorkspaceWrite,
}

impl SandboxMode {
    /// Stable name used in diagnostics and the CLI.
    pub fn label(self) -> &'static str {
        match self {
            SandboxMode::Off => "off",
            SandboxMode::ReadOnly => "read-only",
            SandboxMode::WorkspaceWrite => "workspace-write",
        }
    }

    /// Parse a CLI/config value. Unknown values return `None` so callers can
    /// report the accepted names.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Some(SandboxMode::Off),
            "read-only" | "readonly" | "ro" => Some(SandboxMode::ReadOnly),
            "workspace-write" | "workspace" | "write" => Some(SandboxMode::WorkspaceWrite),
            _ => None,
        }
    }
}

impl Config {
    /// Built-in defaults, including the `coding` profile and the default
    /// model profile.
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
                tools: vec![
                    "read".into(),
                    "bash".into(),
                    "verify".into(),
                    "skill".into(),
                ],
                model_profile: None,
                prompt: None,
                description: Some("read, bash, verify, skill".into()),
                theme: None,
            },
        );
        // Small, tested surface for weak tool callers: six flat tools, one
        // call per model message, and a server-side "must call a tool" nudge.
        cfg.profiles.insert(
            "strict".into(),
            ProfileConfig {
                packs: vec![],
                tools: vec![
                    "read".into(),
                    "grep".into(),
                    "glob".into(),
                    "edit".into(),
                    "bash".into(),
                    "todo".into(),
                ],
                model_profile: Some("strict".into()),
                prompt: None,
                description: Some(
                    "Weak-model defaults: six flat tools, one tool call per message".into(),
                ),
                theme: None,
            },
        );
        // The mini-swe-agent shape: one shell tool and a documented
        // read/edit/test contract, for models that cannot emit JSON calls.
        cfg.profiles.insert(
            "shell".into(),
            ProfileConfig {
                packs: vec![],
                tools: vec!["bash".into()],
                model_profile: Some("shell".into()),
                prompt: None,
                description: Some(
                    "Experimental: bash only; read, edit, and test through commands".into(),
                ),
                theme: None,
            },
        );
        cfg.model_profiles.insert(
            "default".into(),
            ModelProfile {
                // No hard-coded window: probes fill it when available, and
                // `DEFAULT_CONTEXT_WINDOW` is the fallback.
                context_window: None,
                tool_mode: ToolMode::Repair,
                strip_reasoning: Some(true),
                ..ModelProfile::default()
            },
        );
        cfg.model_profiles.insert(
            "strict".into(),
            ModelProfile {
                match_: vec![],
                context_window: None,
                tool_mode: ToolMode::Repair,
                strip_reasoning: Some(true),
                parallel_tool_calls: Some(false),
                tool_choice: Some("required".into()),
                one_call: true,
                short_tool_schemas: Some(true),
                ..ModelProfile::default()
            },
        );
        cfg.model_profiles.insert(
            "shell".into(),
            ModelProfile {
                match_: vec![],
                context_window: None,
                tool_mode: ToolMode::Repair,
                strip_reasoning: Some(true),
                parallel_tool_calls: Some(false),
                tool_choice: None,
                one_call: false,
                short_tool_schemas: Some(true),
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
        if let Some(project) = project
            && project.exists()
        {
            let raw = read_toml(project)?;
            deep_merge(&mut value, raw);
        }
        let mut cfg: Config = value.try_into().context("config schema mismatch")?;
        cfg.apply_env();
        cfg.validate()?;
        Ok(cfg)
    }

    /// Environment overrides. `LH_ENDPOINT`, `LH_MODEL`, `LH_API_KEY`,
    /// `LH_PROFILE`, `LH_MODEL_PROFILE`, `LH_THEME` are read.
    pub fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("LH_ENDPOINT")
            && !v.trim().is_empty()
        {
            self.model.endpoint = v;
        }
        if let Ok(v) = std::env::var("LH_MODEL") {
            self.model.model = v;
        }
        if let Ok(v) = std::env::var("LH_API_KEY") {
            self.model.api_key = v;
        }
        if let Ok(v) = std::env::var("LH_PROFILE")
            && !v.trim().is_empty()
        {
            self.profile_name = v;
        }
        if let Ok(v) = std::env::var("LH_MODEL_PROFILE")
            && !v.trim().is_empty()
        {
            self.model_profile_override = Some(v);
        }
        if let Ok(v) = std::env::var("LH_THEME")
            && !v.trim().is_empty()
        {
            self.ui.theme = v;
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

    /// The active profile name: CLI/`LH_PROFILE`, then `default_profile`, then
    /// the built-in `coding`.
    pub fn profile_name(&self) -> &str {
        if !self.profile_name.is_empty() {
            &self.profile_name
        } else if !self.default_profile.is_empty() {
            &self.default_profile
        } else {
            "coding"
        }
    }

    pub fn profile(&self) -> &ProfileConfig {
        static FALLBACK: std::sync::LazyLock<ProfileConfig> =
            std::sync::LazyLock::new(|| ProfileConfig {
                packs: vec!["core".into(), "coding".into()],
                tools: vec![],
                model_profile: Some("auto".into()),
                prompt: None,
                description: None,
                theme: None,
            });
        self.profiles.get(self.profile_name()).unwrap_or(&FALLBACK)
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

    /// Load extra profiles from `profiles/*.toml`; each file is either a
    /// `[profile.<name>]` table or a bare profile.
    pub fn load_profile_dirs(
        &mut self,
        global_dir: &Path,
        project_dir: Option<&Path>,
    ) -> Result<()> {
        let mut dirs = vec![global_dir.to_path_buf()];
        if let Some(project) = project_dir {
            dirs.push(project.to_path_buf());
        }
        for dir in dirs {
            if !dir.exists() {
                continue;
            }
            let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
                .with_context(|| format!("cannot read profile dir {}", dir.display()))?
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
                .collect();
            files.sort();
            for path in files {
                let raw = read_toml(&path)?;
                if let Some(table) = raw.get("profile").and_then(|value| value.as_table()) {
                    for (name, value) in table {
                        let profile: ProfileConfig =
                            value.clone().try_into().with_context(|| {
                                format!("invalid profile {name:?} in {}", path.display())
                            })?;
                        self.profiles.insert(name.clone(), profile);
                    }
                } else {
                    let name = path
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().to_string())
                        .unwrap_or_else(|| "profile".into());
                    let profile: ProfileConfig = raw
                        .try_into()
                        .with_context(|| format!("invalid profile file {}", path.display()))?;
                    self.profiles.insert(name, profile);
                }
            }
        }
        Ok(())
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

    /// Global prompt customization: inline instructions first, then the
    /// optional file. Both are plain appended text, never code.
    pub fn load_custom_prompt(&self, paths: &crate::paths::Paths) -> Option<String> {
        let mut parts = Vec::new();
        if !self.prompt.instructions.trim().is_empty() {
            parts.push(self.prompt.instructions.trim().to_string());
        }
        let rel = self.prompt.file.as_path();
        if !rel.as_os_str().is_empty() {
            // Config first, then a trusted project, then the raw path. An
            // untrusted repository cannot shadow the configured prompt file.
            let mut candidates = vec![paths.config_dir.join(rel)];
            if self.trust {
                candidates.push(paths.project_dir().join(rel));
            }
            candidates.push(PathBuf::from(rel));
            if let Some(text) = candidates
                .iter()
                .find(|path| path.is_file())
                .and_then(|path| std::fs::read_to_string(path).ok())
            {
                parts.push(text.trim().to_string());
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    /// Optional shell command run once at startup when the endpoint is
    /// unreachable. Empty disables it.
    pub start_command: String,
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
            start_command: String::new(),
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

    /// Persist endpoint, model, and API key under `[model]` in `path`,
    /// preserving every other setting. Used by `/login` and `/model`.
    pub fn save_connection(&self, path: &Path) -> Result<()> {
        use std::io::Write as _;

        let mut value = if path.exists() {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            toml::from_str::<toml::Value>(&raw)
                .with_context(|| format!("invalid TOML in {}", path.display()))?
        } else {
            toml::Value::Table(Default::default())
        };
        let root = value.as_table_mut().context("config root is not a table")?;
        let model = root
            .entry("model".to_string())
            .or_insert_with(|| toml::Value::Table(Default::default()));
        let model = model.as_table_mut().context("[model] is not a table")?;
        model.insert(
            "endpoint".to_string(),
            toml::Value::String(self.endpoint.trim().to_string()),
        );
        model.insert(
            "model".to_string(),
            toml::Value::String(self.model.trim().to_string()),
        );
        model.insert(
            "api_key".to_string(),
            toml::Value::String(self.api_key.clone()),
        );
        let raw = toml::to_string_pretty(&value).context("cannot serialize config")?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        let temporary = path.with_extension("toml.tmp");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .with_context(|| format!("cannot write {}", temporary.display()))?;
        file.write_all(raw.as_bytes())
            .with_context(|| format!("cannot write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("cannot flush {}", temporary.display()))?;
        drop(file);
        std::fs::rename(&temporary, path)
            .with_context(|| format!("cannot replace {}", path.display()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub theme: String,
    pub color_mode: ColorMode,
    pub show_reasoning: bool,
    pub show_token_counts: bool,
    /// Capture the mouse so the wheel scrolls the conversation. Terminal text
    /// selection then needs Shift (Option on macOS) held while dragging.
    pub mouse: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "default".into(),
            color_mode: ColorMode::Auto,
            show_reasoning: false,
            show_token_counts: true,
            mouse: true,
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

/// Global prompt customization. Empty fields keep the built-in text.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptConfig {
    /// Replaces the built-in one-line identity when non-empty.
    pub identity: String,
    /// Extra instructions appended after the built-in operating rules.
    pub instructions: String,
    /// Extra instructions file, resolved like a profile prompt.
    pub file: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LspConfig {
    /// Whether post-mutation diagnostics block a write, warn, or are ignored.
    pub gate: LspGate,
}

/// How a new language-server error after a mutation is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LspGate {
    /// Do not snapshot, check, or attach diagnostics.
    Off,
    /// Attach new errors to the tool result; keep the change.
    #[default]
    Warn,
    /// Restore the pre-edit file and fail with `EDIT_REGRESSION`.
    Block,
}

impl LspGate {
    pub fn label(self) -> &'static str {
        match self {
            LspGate::Off => "off",
            LspGate::Warn => "warn",
            LspGate::Block => "block",
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
    /// Value sent as the request's `tool_choice`; `None` keeps `auto`.
    pub tool_choice: Option<String>,
    /// Reject a second tool call in the same assistant message at dispatch.
    pub one_call: bool,
    /// Strip property descriptions and shorten tool descriptions in requests.
    pub short_tool_schemas: Option<bool>,
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
            tool_choice: None,
            one_call: false,
            short_tool_schemas: None,
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

    pub fn short_tool_schemas(&self) -> bool {
        self.short_tool_schemas.unwrap_or(false)
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
    let value: toml::Value =
        toml::from_str(&raw).with_context(|| format!("invalid TOML in {}", path.display()))?;
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
        assert_eq!(cfg.profile_name(), "coding");
        assert_eq!(cfg.context_window(), 32768);
        assert_eq!(cfg.profile().packs, ["core", "coding"]);
        assert_eq!(cfg.ui.theme, "default");
        assert_eq!(cfg.profiles["full"].packs, ["core", "coding", "security"]);

        let mut cfg = Config::builtin();
        cfg.default_profile = "full".into();
        assert_eq!(cfg.profile_name(), "full");
        cfg.profile_name = "minimal".into();
        assert_eq!(
            cfg.profile_name(),
            "minimal",
            "an explicit selection wins over the config default"
        );
    }

    #[test]
    fn strict_and_shell_profiles_are_discoverable() {
        let cfg = Config::builtin();
        for name in ["strict", "shell"] {
            assert!(
                cfg.profiles.contains_key(name),
                "missing built-in profile {name}"
            );
        }
        let strict = &cfg.profiles["strict"];
        assert_eq!(
            strict.tools,
            ["read", "grep", "glob", "edit", "bash", "todo"]
        );
        assert!(strict.packs.is_empty());
        assert_eq!(strict.model_profile.as_deref(), Some("strict"));

        let mut cfg = Config::builtin();
        cfg.profile_name = "strict".into();
        let (name, profile) = cfg.model_profile();
        assert_eq!(name, "strict");
        assert_eq!(profile.parallel_tool_calls, Some(false));
        assert_eq!(profile.tool_choice.as_deref(), Some("required"));
        assert!(profile.one_call);
        assert!(profile.short_tool_schemas());

        let shell = &cfg.profiles["shell"];
        assert_eq!(shell.tools, ["bash"]);
        cfg.profile_name = "shell".into();
        assert_eq!(cfg.model_profile().0, "shell");

        let registry = crate::tools::ToolRegistry::builtin();
        for name in ["strict", "shell"] {
            let profile = &cfg.profiles[name];
            let selected = registry
                .select(profile, &[])
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert!(!selected.is_empty(), "{name} selects no tools");
        }
    }

    #[test]
    fn lsp_gate_defaults_to_warn_and_parses_names() {
        let cfg = Config::builtin();
        assert_eq!(cfg.lsp.gate, LspGate::Warn);
        let parsed: Config = toml::from_str("[lsp]\ngate = \"block\"\n").expect("block is valid");
        assert_eq!(parsed.lsp.gate, LspGate::Block);
        let parsed: Config = toml::from_str("[lsp]\ngate = \"off\"\n").expect("off is valid");
        assert_eq!(parsed.lsp.gate, LspGate::Off);
        assert!(toml::from_str::<Config>("[lsp]\ngate = \"sometimes\"\n").is_err());
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
    fn profile_directories_load_bare_and_tabled_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let profiles = dir.path().join("profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        std::fs::write(
            profiles.join("review.toml"),
            "packs = [\"core\"]\ntools = [\"read\"]\n",
        )
        .unwrap();
        std::fs::write(
            profiles.join("extra.toml"),
            "[profile.audit]\npacks = [\"core\", \"security\"]\n",
        )
        .unwrap();
        let mut config = Config::builtin();
        config.load_profile_dirs(&profiles, None).unwrap();
        assert_eq!(config.profiles["review"].tools, ["read"]);
        assert_eq!(config.profiles["audit"].packs, ["core", "security"]);
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

    #[test]
    fn default_profile_loads_from_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "default_profile = \"full\"\n").unwrap();
        let cfg = Config::load(&path, None).unwrap();
        assert_eq!(cfg.profile_name(), "full");
        assert_eq!(cfg.profile().packs, ["core", "coding", "security"]);
    }

    #[test]
    fn custom_prompt_combines_inline_text_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = crate::paths::Paths::discover(dir.path()).unwrap();
        paths.config_dir = dir.path().join("config");
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        std::fs::write(paths.config_dir.join("prompt.md"), "From the file.\n").unwrap();

        let mut cfg = Config::builtin();
        cfg.prompt.instructions = "Inline rule.".into();
        cfg.prompt.file = PathBuf::from("prompt.md");
        let text = cfg.load_custom_prompt(&paths).unwrap();
        assert!(text.contains("Inline rule."));
        assert!(text.contains("From the file."));
    }

    #[test]
    fn untrusted_project_cannot_shadow_the_prompt_file() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".harness")).unwrap();
        std::fs::write(project.join(".harness/prompt.md"), "repo text\n").unwrap();
        let mut paths = crate::paths::Paths::discover(&project).unwrap();
        paths.config_dir = dir.path().join("config");

        let mut cfg = Config::builtin();
        cfg.prompt.file = PathBuf::from("prompt.md");
        assert!(cfg.load_custom_prompt(&paths).is_none());

        cfg.trust = true;
        assert_eq!(cfg.load_custom_prompt(&paths).as_deref(), Some("repo text"));
    }

    #[test]
    fn save_connection_preserves_unrelated_settings_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[model]\nendpoint = \"http://old:1\"\nmodel = \"old\"\napi_key = \"old\"\nstart_command = \"server --start\"\n\n[ui]\ntheme = \"light\"\n",
        )
        .unwrap();
        let model = ModelConfig {
            endpoint: "http://new:2".into(),
            model: "new-model".into(),
            api_key: "secret".into(),
            ..ModelConfig::default()
        };
        model.save_connection(&path).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Value = toml::from_str(&raw).unwrap();
        assert_eq!(parsed["model"]["endpoint"].as_str(), Some("http://new:2"));
        assert_eq!(parsed["model"]["model"].as_str(), Some("new-model"));
        assert_eq!(parsed["model"]["api_key"].as_str(), Some("secret"));
        assert_eq!(
            parsed["model"]["start_command"].as_str(),
            Some("server --start")
        );
        assert_eq!(parsed["ui"]["theme"].as_str(), Some("light"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "config with an API key must be private"
            );
        }
    }

    #[test]
    fn save_connection_creates_the_config_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/config.toml");
        let model = ModelConfig {
            endpoint: "http://127.0.0.1:8080".into(),
            model: "m".into(),
            ..ModelConfig::default()
        };
        model.save_connection(&path).unwrap();
        assert!(path.is_file());
    }
}
