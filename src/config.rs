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
    /// Commit and pull-request approval for the `git` and `pr` tools.
    pub git: GitConfig,
    /// Profile used when neither `--profile` nor `TOT_PROFILE` is set.
    pub default_profile: String,
    #[serde(rename = "profile")]
    pub profiles: BTreeMap<String, ProfileConfig>,
    #[serde(rename = "model_profile")]
    pub model_profiles: BTreeMap<String, ModelProfile>,
    pub verify: VerifyConfig,
    /// When file mutations pause for interactive hunk review.
    pub review: ReviewMode,
    /// Settle gates (`[gates]`).
    pub gates: GatesConfig,
    /// Formatting of edited files (`[format]`).
    pub format: FormatConfig,
    /// Skill index in the system prompt (`[skills]`).
    pub skills: SkillsConfig,
    /// The opt-in `fetch` tool (`[fetch]`).
    pub fetch: FetchConfig,
    /// Lifecycle hooks, keyed by hook name (`[hooks.<name>]`).
    pub hooks: HooksConfig,
    /// Model Context Protocol servers, keyed by server name (`[mcp.<name>]`).
    pub mcp: BTreeMap<String, McpServerConfig>,
    /// Image attachments and the optional describing model (`[vision]`).
    pub vision: VisionConfig,
    /// Permission mode and ordered allow/ask/deny rules (`[permissions]`).
    /// Read from the global config and a trusted project config only; rules
    /// from both are kept, global first.
    pub permissions: crate::policy::PermissionsConfig,
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

/// `[fetch]`: settings for the opt-in `fetch` tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FetchConfig {
    /// Hosts (`host` or `host:port`) that may resolve to loopback, private or
    /// link-local addresses. Everything else must be public.
    pub allow_private: Vec<String>,
}

/// `[skills]`: how much of the skill index the system prompt carries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    /// `full` (capped entries), `names` (one line of names), or `off`.
    pub index: crate::skills::IndexMode,
}

/// Settle-gate behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GatesConfig {
    /// Run verify and review from the harness when the model tries to settle,
    /// instead of asking the model to call them. A clean result settles on
    /// the answer already written; a failure continues with one message.
    pub auto_settle: bool,
    /// With no configured or accepted verify steps, use the steps detected
    /// from the project manifest for this session. They run through the
    /// permission policy like any command and are not saved.
    pub verify_detected: bool,
}

impl Default for GatesConfig {
    fn default() -> Self {
        Self {
            auto_settle: true,
            verify_detected: true,
        }
    }
}

/// `[format]`: run the project's formatter on content `edit`, `patch`, and
/// `write` produce, before it is written.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FormatConfig {
    /// Format a file the project already keeps formatted (see
    /// [`crate::tools::format`]).
    pub on_edit: bool,
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self { on_edit: true }
    }
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
        // Opt-in content-hash anchors: `read` annotates lines and `patch`
        // accepts `path#snapshot` plus PUT/CUT/REM/MV operations. Exact-text
        // editing remains the default.
        cfg.model_profiles.insert(
            "hash".into(),
            ModelProfile {
                tool_mode: ToolMode::Repair,
                strip_reasoning: Some(true),
                edit_mode: EditMode::Hash,
                short_tool_schemas: Some(ToolSchemas::Short),
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
                short_tool_schemas: Some(ToolSchemas::Short),
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
                short_tool_schemas: Some(ToolSchemas::Short),
                ..ModelProfile::default()
            },
        );
        // Sampler presets. `model_profile = "auto"` picks one from the model
        // id, so selecting a Qwen/Gemma/GLM model with `/model` also applies
        // its sampler profile. They can be named explicitly with
        // `--model-profile` or `model_profile = "qwen-coding"`.
        cfg.model_profiles.insert(
            "qwen-coding".into(),
            ModelProfile {
                match_: vec![
                    "qwen3-coder".into(),
                    "qwen3.5-coder".into(),
                    "qwen2.5-coder".into(),
                    "qwen3".into(),
                ],
                temperature: Some(0.2),
                top_p: Some(0.95),
                min_p: Some(0.05),
                repeat_penalty: Some(1.05),
                dry_multiplier: Some(0.8),
                dry_base: Some(1.75),
                xtc_probability: Some(0.0),
                seed: Some(0),
                tool_mode: ToolMode::Auto,
                ..ModelProfile::default()
            },
        );
        cfg.model_profiles.insert(
            "gemma-chat".into(),
            ModelProfile {
                match_: vec![
                    "gemma3".into(),
                    "gemma-3".into(),
                    "gemma2".into(),
                    "gemma".into(),
                ],
                temperature: Some(0.7),
                top_p: Some(0.95),
                min_p: Some(0.0),
                repeat_penalty: Some(1.1),
                seed: Some(0),
                tool_mode: ToolMode::Auto,
                ..ModelProfile::default()
            },
        );
        cfg.model_profiles.insert(
            "glm-agent".into(),
            ModelProfile {
                match_: vec!["glm-4".into(), "glm4".into(), "glm".into()],
                temperature: Some(0.6),
                top_p: Some(0.9),
                min_p: Some(0.02),
                repeat_penalty: Some(1.05),
                dry_multiplier: Some(0.8),
                dry_base: Some(1.75),
                xtc_probability: Some(0.0),
                seed: Some(0),
                tool_mode: ToolMode::Auto,
                ..ModelProfile::default()
            },
        );
        cfg
    }

    /// Load defaults, then the global config, then an optional trusted project
    /// config. `project` is only passed when trust is granted.
    pub fn load(global: &Path, project: Option<&Path>) -> Result<Self> {
        let mut value = toml::Value::try_from(Self::builtin()).context("serialize defaults")?;
        let mut rules: Vec<toml::Value> = Vec::new();
        if global.exists() {
            let mut raw = read_toml(global)?;
            rules.extend(take_permission_rules(&mut raw));
            deep_merge(&mut value, raw);
        }
        if let Some(project) = project
            && project.exists()
        {
            let mut raw = read_toml(project)?;
            rules.extend(take_permission_rules(&mut raw));
            deep_merge(&mut value, raw);
        }
        // Arrays replace on merge; permission rules accumulate instead, so a
        // trusted project adds rules after the global ones rather than
        // silently dropping them.
        if let Some(table) = value.as_table_mut() {
            let permissions = table
                .entry("permissions")
                .or_insert_with(|| toml::Value::Table(Default::default()));
            if let Some(permissions) = permissions.as_table_mut() {
                permissions.insert("rules".into(), toml::Value::Array(rules));
            }
        }
        let mut cfg: Config = value.try_into().context("config schema mismatch")?;
        cfg.apply_env();
        cfg.validate()?;
        Ok(cfg)
    }

    /// Environment overrides. `TOT_ENDPOINT`, `TOT_MODEL`, `TOT_API_KEY`,
    /// `TOT_PROFILE`, `TOT_MODEL_PROFILE`, `TOT_THEME`, and `TOT_AGENTS_MD` are
    /// read.
    pub fn apply_env(&mut self) {
        if let Ok(v) = crate::paths::env_var("ENDPOINT")
            && !v.trim().is_empty()
        {
            self.model.endpoint = v;
        }
        if let Ok(v) = crate::paths::env_var("MODEL") {
            self.model.model = v;
        }
        if let Ok(v) = crate::paths::env_var("API_KEY") {
            self.model.api_key = v;
        }
        if let Ok(v) = crate::paths::env_var("PROFILE")
            && !v.trim().is_empty()
        {
            self.profile_name = v;
        }
        if let Ok(v) = crate::paths::env_var("MODEL_PROFILE")
            && !v.trim().is_empty()
        {
            self.model_profile_override = Some(v);
        }
        if let Ok(v) = crate::paths::env_var("THEME")
            && !v.trim().is_empty()
        {
            self.ui.theme = v;
        }
        if let Ok(value) = crate::paths::env_var("AGENTS_MD") {
            match value.trim().to_ascii_lowercase().as_str() {
                "0" | "false" | "off" | "no" => self.prompt.agents_md = false,
                "1" | "true" | "on" | "yes" => self.prompt.agents_md = true,
                _ => {}
            }
        }
        if crate::paths::env_var("NO_SESSION").is_ok() {
            self.session.no_session = true;
        }
        if crate::paths::env_var("TRUST").is_ok() {
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
        // Zero disables these two; 1 would end a turn on the very first call.
        for (key, value) in [
            ("max_same_tool_family", self.budgets.max_same_tool_family),
            (
                "max_same_tool_family_success",
                self.budgets.max_same_tool_family_success,
            ),
        ] {
            if value == 1 {
                bail!("budgets.{key} must be 0 (disabled) or at least 2");
            }
        }
        if self.model.context_window == Some(0) {
            bail!("model.context_window must be greater than 0");
        }
        if self.model.max_output_tokens == 0 {
            bail!("model.max_output_tokens must be greater than 0");
        }
        for (name, server) in &self.mcp {
            server
                .validate(name)
                .with_context(|| format!("invalid [mcp.{name}]"))?;
        }
        self.hooks.validate()?;
        if self.permissions.mode == crate::policy::PermissionMode::Yolo {
            bail!(
                "permissions.mode = \"yolo\" is not accepted in a config file; pass --yolo explicitly for one run"
            );
        }
        crate::policy::Policy::compile(&self.permissions.rules).map_err(anyhow::Error::msg)?;
        Ok(())
    }

    /// The active profile name: CLI/`TOT_PROFILE`, then `default_profile`, then
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
        // A user profile always beats a built-in sampler preset, even when
        // both match the same prefix. Presets still apply when the user has
        // not configured a matching profile.
        const PRESETS: &[&str] = &["qwen-coding", "gemma-chat", "glm-agent"];
        for skip_presets in [true, false] {
            for (name, profile) in &self.model_profiles {
                if name == "default" || (skip_presets && PRESETS.contains(&name.as_str())) {
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

    /// Write one setting under `[section]` in a config file, preserving every
    /// unrelated key. The file is replaced atomically; its mode is kept, and a
    /// new file is created `0600` on Unix.
    pub fn save_setting(path: &Path, section: &str, key: &str, value: toml::Value) -> Result<()> {
        let mut file = read_config_value(path)?;
        {
            let root = file.as_table_mut().context("config root is not a table")?;
            let table = root
                .entry(section.to_string())
                .or_insert_with(|| toml::Value::Table(Default::default()));
            let table = table
                .as_table_mut()
                .with_context(|| format!("[{section}] is not a table"))?;
            table.insert(key.to_string(), value);
        }
        write_config_value(path, &file)
    }

    /// Persist one `[ui]` setting, used by `/theme` and `Ctrl+R`.
    pub fn save_ui_setting(path: &Path, key: &str, value: toml::Value) -> Result<()> {
        Self::save_setting(path, "ui", key, value)
    }

    /// Append one rule to `[[permissions.rules]]` in the global config,
    /// preserving every other key. A rule with the same id is not added twice.
    pub fn save_permission_rule(path: &Path, rule: &crate::policy::RuleConfig) -> Result<()> {
        let mut file = read_config_value(path)?;
        {
            let root = file.as_table_mut().context("config root is not a table")?;
            let permissions = root
                .entry("permissions")
                .or_insert_with(|| toml::Value::Table(Default::default()))
                .as_table_mut()
                .context("[permissions] is not a table")?;
            let rules = permissions
                .entry("rules")
                .or_insert_with(|| toml::Value::Array(Vec::new()))
                .as_array_mut()
                .context("permissions.rules is not an array")?;
            let exists = rule.id.as_deref().is_some_and(|id| {
                rules
                    .iter()
                    .any(|existing| existing.get("id").and_then(|v| v.as_str()) == Some(id))
            });
            if !exists {
                rules.push(toml::Value::try_from(rule).context("cannot encode the rule")?);
            }
        }
        write_config_value(path, &file)
    }

    /// Persist the startup profile `default_profile`, preserving every other
    /// key. `--profile` and `TOT_PROFILE` still win for the current run.
    pub fn save_default_profile(path: &Path, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            bail!("default profile name is empty");
        }
        let mut file = read_config_value(path)?;
        {
            let root = file.as_table_mut().context("config root is not a table")?;
            root.insert(
                "default_profile".into(),
                toml::Value::String(name.to_string()),
            );
        }
        write_config_value(path, &file)
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
    /// Engine fingerprint: `auto` (empty) or a name from `tot doctor`
    /// (`llama.cpp`, `vllm`, `sglang`, `ollama`, `lmstudio`, `koboldcpp`,
    /// `tabbyapi`, `mlx`, `generic`). Pinning it skips the metadata probes and
    /// decides sampler and structured-output wire formats behind a gateway.
    pub engine: String,
    pub temperature: f32,
    pub top_p: f32,
    /// Global sampler defaults; a model profile overrides each field it sets.
    pub min_p: Option<f32>,
    pub repeat_penalty: Option<f32>,
    pub dry_multiplier: Option<f32>,
    pub dry_base: Option<f32>,
    pub xtc_probability: Option<f32>,
    pub seed: Option<i64>,
    /// Path to a JSON Schema that constrains the final answer. Empty disables
    /// structured output. `--json-schema` overrides it.
    pub response_schema: String,
    pub max_output_tokens: u32,
    pub request_timeout_secs: u64,
    pub connect_timeout_secs: u64,
    pub first_token_timeout_secs: u64,
    pub stream_idle_timeout_secs: u64,
    /// Times a model call that failed for a transient reason (connection,
    /// 5xx, 429, a dropped or stalled stream) is sent again within the turn.
    /// 0 ends the turn on the first failure.
    pub retries: u32,
    /// Longest total wait across those retries, `Retry-After` included.
    pub retry_deadline_secs: u64,
    /// First backoff step; later steps are 4x and 15x this, with jitter.
    pub retry_backoff_ms: u64,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.into(),
            model: String::new(),
            api_key: String::new(),
            start_command: String::new(),
            context_window: None,
            engine: String::new(),
            temperature: 0.2,
            top_p: 0.95,
            min_p: None,
            repeat_penalty: None,
            dry_multiplier: None,
            dry_base: None,
            xtc_probability: None,
            seed: None,
            response_schema: String::new(),
            max_output_tokens: 4096,
            request_timeout_secs: 900,
            connect_timeout_secs: 5,
            first_token_timeout_secs: 120,
            stream_idle_timeout_secs: 120,
            retries: 3,
            retry_deadline_secs: 120,
            retry_backoff_ms: 1000,
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
        let mut value = read_config_value(path)?;
        {
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
        }
        write_private_config_value(path, &value)
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
    /// Redact secret values before session entries, artifacts, evidence, and
    /// reports are written. `--no-redact` disables it for one session.
    pub redact: bool,
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
            redact: true,
            artifact_retention_days: 30,
            keep_artifacts: false,
            clean_artifacts: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReviewMode {
    /// Review every mutation before it is written.
    Always,
    /// Review once a turn is about to touch a second file.
    #[default]
    MultiFile,
    /// Write immediately, exactly as before the review feature.
    Off,
}

impl ReviewMode {
    pub fn label(self) -> &'static str {
        match self {
            ReviewMode::Always => "always",
            ReviewMode::MultiFile => "multi_file",
            ReviewMode::Off => "off",
        }
    }

    /// Parse a config or CLI value; unknown values return `None` so callers
    /// can name the accepted ones.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "always" => Some(ReviewMode::Always),
            "multi_file" | "multi-file" | "multi" => Some(ReviewMode::MultiFile),
            "off" | "never" | "none" => Some(ReviewMode::Off),
            _ => None,
        }
    }
}

/// Lifecycle hooks. Each `[hooks.<name>]` table names one event and one
/// directly executed command. `timeout_secs` and `max_output_bytes` are the
/// defaults inherited by every hook.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HooksConfig {
    /// Per-hook deadline when the hook does not set its own.
    pub timeout_secs: u64,
    /// Recorded output cap. Larger output is archived and referenced.
    pub max_output_bytes: usize,
    #[serde(flatten)]
    pub entries: BTreeMap<String, HookConfig>,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 5,
            max_output_bytes: 16 * 1024,
            entries: BTreeMap::new(),
        }
    }
}

impl HooksConfig {
    pub fn hooks_for(&self, event: HookEvent) -> impl Iterator<Item = (&String, &HookConfig)> {
        self.entries
            .iter()
            .filter(move |(_, hook)| hook.event == event)
    }

    pub fn timeout(&self, hook: &HookConfig) -> Duration {
        Duration::from_secs(if hook.timeout_secs > 0 {
            hook.timeout_secs
        } else {
            self.timeout_secs.max(1)
        })
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.timeout_secs == 0 {
            bail!("hooks.timeout_secs must be at least 1 second");
        }
        if self.max_output_bytes < 256 {
            bail!("hooks.max_output_bytes must be at least 256");
        }
        for (name, hook) in &self.entries {
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                bail!("hook names may only use letters, digits, '-' and '_'");
            }
            if hook.command.is_empty() || hook.command[0].trim().is_empty() {
                bail!("hook {name:?}: set `command` to the executable and its arguments");
            }
        }
        Ok(())
    }
}

/// One lifecycle hook.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HookConfig {
    pub event: HookEvent,
    /// Program plus arguments. Spawned directly; never through a shell.
    #[serde(default)]
    pub command: Vec<String>,
    /// Per-hook deadline; 0 inherits `[hooks] timeout_secs`.
    #[serde(default)]
    pub timeout_secs: u64,
    /// `block` fails closed when the hook cannot run or answers badly.
    /// `warn` makes the failure visible and continues. The default is
    /// `block` for `pre_tool` and `warn` for every other event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_error: Option<HookOnError>,
}

impl HookConfig {
    pub fn on_error(&self) -> HookOnError {
        self.on_error
            .unwrap_or_else(|| self.event.default_on_error())
    }
}

/// Where a hook runs in the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    SessionStart,
    UserPrompt,
    PreTool,
    PostTool,
    PreCompact,
    Stop,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "session_start",
            HookEvent::UserPrompt => "user_prompt",
            HookEvent::PreTool => "pre_tool",
            HookEvent::PostTool => "post_tool",
            HookEvent::PreCompact => "pre_compact",
            HookEvent::Stop => "stop",
        }
    }

    pub fn default_on_error(self) -> HookOnError {
        match self {
            HookEvent::PreTool => HookOnError::Block,
            _ => HookOnError::Warn,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HookOnError {
    /// A hook that cannot run or answers badly blocks the action.
    #[default]
    Block,
    /// A hook that cannot run or answers badly is visible but does not block.
    Warn,
}

impl HookOnError {
    pub fn label(self) -> &'static str {
        match self {
            HookOnError::Block => "block",
            HookOnError::Warn => "warn",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetConfig {
    /// Tool-call checkpoint interval. Productive runs renew; a run with no
    /// new result or file change in the last `stall_calls` calls stops.
    pub max_tool_calls: u32,
    /// Model-call checkpoint interval, with the same renewal rule.
    pub max_model_calls: u32,
    /// Wall-clock checkpoint interval, with the same renewal rule.
    pub max_wall_minutes: u32,
    /// Consecutive identical calls and the no-progress window used by the
    /// checkpoints.
    pub stall_calls: u32,
    /// Failing or repeated calls of one tool family (same program and
    /// operation, or same tool and key arguments) allowed in a turn before it
    /// stops as stalled. Zero disables the guard. Read-only calls are not
    /// counted as repeats; a read repeated with the same output is caught by
    /// `stall_calls`.
    pub max_same_tool_family: u32,
    /// Successful calls of one tool family that changed no workspace file and
    /// passed no verify allowed in a turn before it stops as stalled. A
    /// same-family success does not reset the count. Read-only calls are
    /// exempt. Zero disables the guard.
    pub max_same_tool_family_success: u32,
    pub verify_attempts: u32,
    /// Characters one tool result may occupy in context. Unset scales with
    /// the usable prompt (4%, clamped to 2500..16000).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_cap_chars: Option<usize>,
    /// Characters one batch of tool results may occupy. Unset scales with
    /// the usable prompt (12%, clamped to 8000..40000).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_cap_chars: Option<usize>,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            max_tool_calls: 120,
            max_model_calls: 40,
            max_wall_minutes: 30,
            stall_calls: 12,
            max_same_tool_family: 8,
            max_same_tool_family_success: 24,
            verify_attempts: 2,
            result_cap_chars: None,
            batch_cap_chars: None,
        }
    }
}

/// Global prompt customization. Empty fields keep the built-in text.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptConfig {
    /// Replaces the built-in one-line identity when non-empty.
    pub identity: String,
    /// Extra instructions appended after the built-in operating rules.
    pub instructions: String,
    /// Extra instructions file, resolved like a profile prompt.
    pub file: PathBuf,
    /// Load the hierarchical `AGENTS.md` chain into the system prompt.
    /// `--no-agents-md` and `TOT_AGENTS_MD=0` disable it.
    pub agents_md: bool,
    /// Attach files a prompt names without `@` (`src/a.rs:12`, a pasted
    /// stack trace), like `@` mentions; see `agent::mentions`.
    pub attach_named_files: bool,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            identity: String::new(),
            instructions: String::new(),
            file: PathBuf::new(),
            agents_md: true,
            attach_named_files: true,
        }
    }
}

/// One Model Context Protocol server. Exactly one of `command` (stdio) or
/// `url` (Streamable HTTP) is set. `${VAR}` in `args`, `env`, `url`, and
/// `headers` expands from the environment so secrets stay out of the file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct McpServerConfig {
    /// Executable for a stdio server.
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Working directory for a stdio server; the project directory when empty.
    pub cwd: PathBuf,
    /// Endpoint for a Streamable HTTP server.
    pub url: String,
    pub headers: BTreeMap<String, String>,
    /// `false` keeps the server configured but never started.
    pub enabled: bool,
    /// Profiles that attach every tool of this server. Unset attaches the
    /// server to every profile; `[]` attaches it only where a profile names
    /// `mcp:<name>` or one of its tools.
    pub profiles: Option<Vec<String>>,
    /// Remote tool names to expose; empty exposes all.
    pub tools: Vec<String>,
    /// Remote tool names to hide.
    pub exclude_tools: Vec<String>,
    /// Handshake and tool-listing deadline.
    pub startup_timeout_secs: u64,
    /// Deadline for one `tools/call`.
    pub timeout_secs: u64,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: PathBuf::new(),
            url: String::new(),
            headers: BTreeMap::new(),
            enabled: true,
            profiles: None,
            tools: Vec::new(),
            exclude_tools: Vec::new(),
            startup_timeout_secs: 30,
            timeout_secs: 120,
        }
    }
}

impl McpServerConfig {
    fn validate(&self, name: &str) -> Result<()> {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            bail!("server names may only use letters, digits, '-' and '_'");
        }
        match (self.command.trim().is_empty(), self.url.trim().is_empty()) {
            (true, true) => bail!("set `command` for a stdio server or `url` for an HTTP server"),
            (false, false) => bail!("set either `command` or `url`, not both"),
            _ => {}
        }
        if self.timeout_secs == 0 || self.startup_timeout_secs == 0 {
            bail!("timeouts must be at least 1 second");
        }
        Ok(())
    }

    /// Whether this server attaches all of its tools to `profile` without the
    /// profile naming it.
    pub fn attaches_to(&self, profile: &str) -> bool {
        match &self.profiles {
            None => true,
            Some(profiles) => profiles.iter().any(|p| p == profile),
        }
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }

    pub fn startup_timeout(&self) -> Duration {
        Duration::from_secs(self.startup_timeout_secs)
    }
}

/// `[git]`: who approves the high-consequence git actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GitConfig {
    /// `git commit` from the model: `ask` waits for `/commit confirm` in the
    /// TUI; `allow` commits at once (unattended runs).
    pub commit: GitApproval,
    /// Draft PRs and PR comments: `ask` waits for `/pr confirm`.
    pub pr: GitApproval,
    /// Longest accepted commit subject, in characters.
    pub subject_max: usize,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            commit: GitApproval::Ask,
            pr: GitApproval::Ask,
            subject_max: 72,
        }
    }
}

/// Whether a git action runs at once or waits for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GitApproval {
    /// Record a preview; the user approves it with a slash command.
    #[default]
    Ask,
    /// Run immediately.
    Allow,
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
    /// How file edits are anchored: exact text (default) or content hashes.
    pub edit_mode: EditMode,
    /// Strip property descriptions and shorten tool descriptions in requests:
    /// `true`, `false`, or `"auto"` (short when the static prompt would take
    /// more than [`AUTO_SHORT_SCHEMA_SHARE`] of the usable prompt).
    pub short_tool_schemas: Option<ToolSchemas>,
    pub stop: Vec<String>,
    pub extra_body: Option<toml::Value>,
    /// Sampler values for this model. Each `Some` value overrides the global
    /// `[model]` field; fields unsupported by the detected engine are omitted
    /// and reported by `tot doctor`.
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub min_p: Option<f32>,
    pub repeat_penalty: Option<f32>,
    pub dry_multiplier: Option<f32>,
    pub dry_base: Option<f32>,
    pub xtc_probability: Option<f32>,
    pub seed: Option<i64>,
    /// Hint for tool schema style; the harness always sends JSON schema.
    pub native_tools: Option<bool>,
    /// The model accepts images as content parts. `None` asks the server
    /// (llama.cpp `/props`), else assumes text only.
    pub vision: Option<bool>,
    /// Whether the model should think: `auto`, `on`, or `off`.
    pub thinking: ThinkingPolicy,
    /// Request fields that turn thinking off on this engine, merged into
    /// `extra_body` when the resolved policy is off. For example
    /// `{ chat_template_kwargs = { enable_thinking = false } }`.
    pub thinking_control: Option<toml::Value>,
    /// The model cannot call tools reliably while thinking; `thinking =
    /// "auto"` then turns thinking off whenever tools are active.
    pub thinking_breaks_tools: bool,
}

/// `thinking` in a model profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingPolicy {
    #[default]
    Auto,
    On,
    Off,
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
            edit_mode: EditMode::Text,
            short_tool_schemas: None,
            stop: vec![],
            extra_body: None,
            temperature: None,
            top_p: None,
            min_p: None,
            repeat_penalty: None,
            dry_multiplier: None,
            dry_base: None,
            xtc_probability: None,
            seed: None,
            native_tools: None,
            vision: None,
            thinking: ThinkingPolicy::Auto,
            thinking_control: None,
            thinking_breaks_tools: false,
        }
    }
}

/// Deep-merge `patch` into `base`; objects merge key by key, anything else
/// replaces.
fn merge_json(base: &mut serde_json::Value, patch: serde_json::Value) {
    match (base, patch) {
        (serde_json::Value::Object(base), serde_json::Value::Object(patch)) => {
            for (key, value) in patch {
                merge_json(base.entry(key).or_insert(serde_json::Value::Null), value);
            }
        }
        (base, patch) => *base = patch,
    }
}

/// Stream fields the decoder reads reasoning from, in fallback order after a
/// configured `reasoning_field`.
pub const REASONING_FIELDS: [&str; 3] = ["reasoning_content", "reasoning", "thinking"];

impl ModelProfile {
    pub fn strip_reasoning(&self) -> bool {
        self.strip_reasoning.unwrap_or(true)
    }

    /// Whether thinking is off for a request: `off`, or `auto` with tools
    /// active on a model marked `thinking_breaks_tools`.
    pub fn thinking_off(&self, tools_active: bool) -> bool {
        match self.thinking {
            ThinkingPolicy::Off => true,
            ThinkingPolicy::On => false,
            ThinkingPolicy::Auto => self.thinking_breaks_tools && tools_active,
        }
    }

    /// The request's `extra_body` with the thinking control merged in even
    /// when the policy would leave thinking on. Used for one retry after a
    /// length stop spent the whole output budget thinking. `None` when the
    /// profile configures no control.
    pub fn request_extra_body_thinking_off(&self) -> Option<serde_json::Value> {
        let control = self
            .thinking_control
            .as_ref()
            .and_then(|value| serde_json::to_value(value).ok())?;
        let mut base = self
            .extra_body
            .as_ref()
            .and_then(|value| serde_json::to_value(value).ok())
            .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
        merge_json(&mut base, control);
        Some(base)
    }

    /// The request's `extra_body`: the profile's, with `thinking_control`
    /// merged on top when thinking is off. Refuses when thinking must be off
    /// but the profile names no control, since sending the request anyway
    /// would silently ignore the policy.
    pub fn request_extra_body(
        &self,
        tools_active: bool,
    ) -> Result<Option<serde_json::Value>, String> {
        let base = self
            .extra_body
            .as_ref()
            .and_then(|value| serde_json::to_value(value).ok());
        if !self.thinking_off(tools_active) {
            return Ok(base);
        }
        let Some(control) = self
            .thinking_control
            .as_ref()
            .and_then(|value| serde_json::to_value(value).ok())
        else {
            let why = if self.thinking == ThinkingPolicy::Off {
                "thinking = \"off\"".to_string()
            } else {
                "thinking = \"auto\" with thinking_breaks_tools = true and tools active".to_string()
            };
            return Err(format!(
                "{why} needs the engine's control to turn thinking off, and this model profile sets no thinking_control. Add it to the model profile, for example `thinking_control = {{ chat_template_kwargs = {{ enable_thinking = false }} }}` (vLLM/SGLang/llama.cpp Qwen3) or `thinking_control = {{ think = false }}` (Ollama), or set thinking = \"on\"."
            ));
        };
        let mut merged = base.unwrap_or_else(|| serde_json::Value::Object(Default::default()));
        merge_json(&mut merged, control);
        Ok(Some(merged))
    }

    /// A startup warning when `reasoning_field` names a field the decoder
    /// never reads, so a typo does not silently hide all reasoning.
    pub fn reasoning_field_warning(&self) -> Option<String> {
        let field = self.reasoning_field.as_deref()?.trim();
        (!REASONING_FIELDS.contains(&field)).then(|| {
            format!(
                "model profile reasoning_field = {field:?} is not a known stream field; accepted: {}. Falling back to the default order.",
                REASONING_FIELDS.join(", ")
            )
        })
    }

    pub fn short_tool_schemas(&self) -> bool {
        self.short_tool_schemas == Some(ToolSchemas::Short)
    }

    /// The configured schema form; unset is full.
    pub fn tool_schemas(&self) -> ToolSchemas {
        self.short_tool_schemas.unwrap_or(ToolSchemas::Full)
    }
}

/// Share of the usable prompt above which `short_tool_schemas = "auto"`
/// sends the short schemas.
pub const AUTO_SHORT_SCHEMA_SHARE: f64 = 0.20;

/// Which tool-schema form requests carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSchemas {
    Full,
    Short,
    /// Decided once per session and model from the static prompt's share of
    /// the usable prompt, so the request prefix stays byte-stable.
    Auto,
}

impl Serialize for ToolSchemas {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            ToolSchemas::Full => serializer.serialize_bool(false),
            ToolSchemas::Short => serializer.serialize_bool(true),
            ToolSchemas::Auto => serializer.serialize_str("auto"),
        }
    }
}

impl<'de> Deserialize<'de> for ToolSchemas {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bool(bool),
            Text(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Bool(true) => Ok(ToolSchemas::Short),
            Raw::Bool(false) => Ok(ToolSchemas::Full),
            Raw::Text(text) => match text.trim().to_ascii_lowercase().as_str() {
                "auto" => Ok(ToolSchemas::Auto),
                "true" | "short" => Ok(ToolSchemas::Short),
                "false" | "full" => Ok(ToolSchemas::Full),
                other => Err(serde::de::Error::custom(format!(
                    "short_tool_schemas must be true, false, or \"auto\", got {other:?}"
                ))),
            },
        }
    }
}

/// `[vision]`: how images reach a model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VisionConfig {
    /// Model that describes images for a model without vision input. Empty
    /// disables the fallback.
    pub model: String,
    /// Endpoint for `model`; empty uses `[model] endpoint`.
    pub endpoint: String,
    /// API key for `endpoint`; empty reuses `[model] api_key` when the
    /// endpoint is shared.
    pub api_key: String,
    /// Allow sending images to an endpoint that is not loopback or on a
    /// private network.
    pub allow_remote: bool,
    /// Largest image accepted, in bytes.
    pub max_bytes: u64,
    /// Send images with their EXIF/XMP metadata instead of stripping it.
    pub keep_exif: bool,
}

impl Default for VisionConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            endpoint: String::new(),
            api_key: String::new(),
            allow_remote: false,
            max_bytes: crate::media::image::DEFAULT_MAX_BYTES,
            keep_exif: false,
        }
    }
}

/// How `read` and `patch` anchor an edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EditMode {
    /// Exact search text; unchanged default.
    #[default]
    Text,
    /// `read` annotates lines with content hashes and `patch` takes
    /// `path#snapshot` with PUT/CUT/REM/MV operations.
    Hash,
}

impl EditMode {
    pub fn label(self) -> &'static str {
        match self {
            EditMode::Text => "text",
            EditMode::Hash => "hash",
        }
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
    /// Manifest files the steps were detected from, when nobody configured
    /// or accepted them. Empty for configured steps. Never read from config.
    #[serde(skip)]
    pub detected_from: Vec<String>,
}

impl VerifyConfig {
    /// Whether the steps are this session's detection, not a saved choice.
    pub fn is_detected(&self) -> bool {
        !self.steps.is_empty() && !self.detected_from.is_empty()
    }
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

/// Read a config file for editing; a missing file is an empty table.
fn read_config_value(path: &Path) -> Result<toml::Value> {
    if !path.exists() {
        return Ok(toml::Value::Table(Default::default()));
    }
    read_toml(path)
}

/// Serialize `value` and replace `path` atomically.
fn write_config_value(path: &Path, value: &toml::Value) -> Result<()> {
    write_config_with_mode(path, value, None)
}

/// Write a config that may carry a secret: the result is always private
/// (`0600`), matching the old `save_connection` behavior.
fn write_private_config_value(path: &Path, value: &toml::Value) -> Result<()> {
    write_config_with_mode(path, value, Some(0o600))
}

fn write_config_with_mode(path: &Path, value: &toml::Value, mode: Option<u32>) -> Result<()> {
    let raw = toml::to_string_pretty(value).context("cannot serialize config")?;
    write_config_atomic(path, &raw, mode)
}

/// Replace `path` with `raw` atomically. An existing file keeps its mode
/// unless `mode` forces one; a new file is created `0600` on Unix. The
/// temporary file lives beside the target so the rename cannot cross
/// filesystems.
fn write_config_atomic(path: &Path, raw: &str, mode: Option<u32>) -> Result<()> {
    use std::io::Write as _;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    #[cfg(unix)]
    let mode = mode
        .or_else(|| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(path)
                .map(|metadata| metadata.permissions().mode() & 0o777)
                .ok()
        })
        .unwrap_or(0o600);
    #[cfg(not(unix))]
    let _ = mode;
    let temporary = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("cannot write {}", temporary.display()))?;
    file.write_all(raw.as_bytes())
        .with_context(|| format!("cannot write {}", temporary.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // A leftover temporary may carry another mode; force the target's.
        file.set_permissions(std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("cannot set permissions on {}", temporary.display()))?;
    }
    file.sync_all()
        .with_context(|| format!("cannot flush {}", temporary.display()))?;
    drop(file);
    std::fs::rename(&temporary, path)
        .with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

/// Remove and return `permissions.rules` from one config file's table.
fn take_permission_rules(raw: &mut toml::Value) -> Vec<toml::Value> {
    raw.get_mut("permissions")
        .and_then(toml::Value::as_table_mut)
        .and_then(|table| table.remove("rules"))
        .map(|rules| match rules {
            toml::Value::Array(rules) => rules,
            // Not an array: keep it so the schema check reports it.
            other => vec![other],
        })
        .unwrap_or_default()
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
        assert!(cfg.session.redact, "secret redaction is on by default");
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
    fn sampler_presets_are_selectable_by_model_and_by_name() {
        let mut cfg = Config::builtin();
        for (model, preset, minimum) in [
            ("qwen3-coder-30b-a3b", "qwen-coding", 0.05),
            ("gemma-3-27b-it", "gemma-chat", 0.0),
            ("glm-4.6", "glm-agent", 0.02),
        ] {
            cfg.model.model = model.into();
            let (name, profile) = cfg.model_profile();
            assert_eq!(name, preset, "{model}");
            assert_eq!(profile.min_p, Some(minimum), "{model}");
            assert_eq!(profile.seed, Some(0), "{model}");
        }
        cfg.model.model = "gemma-3-27b-it".into();
        cfg.model_profile_override = Some("qwen-coding".into());
        assert_eq!(cfg.model_profile().0, "qwen-coding");
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
        // The setting takes a bool or "auto"; unset stays the full form.
        let parse = |text: &str| -> ModelProfile { toml::from_str(text).unwrap() };
        assert_eq!(
            parse("short_tool_schemas = true").tool_schemas(),
            ToolSchemas::Short
        );
        assert_eq!(
            parse("short_tool_schemas = false").tool_schemas(),
            ToolSchemas::Full
        );
        assert_eq!(
            parse("short_tool_schemas = \"auto\"").tool_schemas(),
            ToolSchemas::Auto
        );
        assert_eq!(parse("").tool_schemas(), ToolSchemas::Full);
        assert!(toml::from_str::<ModelProfile>("short_tool_schemas = \"maybe\"").is_err());

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
    fn edit_mode_defaults_to_text_and_parses_hash() {
        let cfg = Config::builtin();
        assert_eq!(cfg.model_profiles["default"].edit_mode, EditMode::Text);
        assert_eq!(cfg.model_profiles["strict"].edit_mode, EditMode::Text);
        assert_eq!(cfg.model_profiles["hash"].edit_mode, EditMode::Hash);

        let parsed: Config = toml::from_str("[model_profile.custom]\nedit_mode = \"hash\"\n")
            .expect("hash is valid");
        assert_eq!(parsed.model_profiles["custom"].edit_mode, EditMode::Hash);
        let parsed: Config = toml::from_str("[model_profile.custom]\nedit_mode = \"text\"\n")
            .expect("text is valid");
        assert_eq!(parsed.model_profiles["custom"].edit_mode, EditMode::Text);
        assert!(
            toml::from_str::<Config>("[model_profile.custom]\nedit_mode = \"fuzzy\"\n").is_err()
        );
    }

    #[test]
    fn git_approval_defaults_to_ask() {
        let cfg = Config::builtin();
        assert_eq!(cfg.git.commit, GitApproval::Ask);
        assert_eq!(cfg.git.pr, GitApproval::Ask);
        assert_eq!(cfg.git.subject_max, 72);
        let parsed: Config = toml::from_str("[git]\ncommit = \"allow\"\n").expect("allow is valid");
        assert_eq!(parsed.git.commit, GitApproval::Allow);
        assert_eq!(parsed.git.pr, GitApproval::Ask);
        assert!(toml::from_str::<Config>("[git]\ncommit = \"yes\"\n").is_err());
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
    fn thinking_policy_resolves_and_merges_the_control() {
        let parsed: Config = toml::from_str(
            r#"
[model_profile.qwen]
thinking = "off"
extra_body = { top_k = 20, chat_template_kwargs = { other = 1 } }
thinking_control = { chat_template_kwargs = { enable_thinking = false } }
"#,
        )
        .expect("valid profile");
        let profile = &parsed.model_profiles["qwen"];
        assert_eq!(profile.thinking, ThinkingPolicy::Off);
        let body = profile.request_extra_body(false).unwrap().unwrap();
        assert_eq!(body["top_k"], 20);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(body["chat_template_kwargs"]["other"], 1);

        // `on` and plain `auto` leave extra_body alone.
        let mut on = profile.clone();
        on.thinking = ThinkingPolicy::On;
        let body = on.request_extra_body(true).unwrap().unwrap();
        assert!(
            body["chat_template_kwargs"]
                .get("enable_thinking")
                .is_none()
        );
        let mut auto = profile.clone();
        auto.thinking = ThinkingPolicy::Auto;
        assert!(!auto.thinking_off(true));
        // `auto` turns it off only with tools active on a marked model.
        auto.thinking_breaks_tools = true;
        assert!(!auto.thinking_off(false));
        let body = auto.request_extra_body(true).unwrap().unwrap();
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);

        // `off` without a control refuses and names the key.
        let bare = ModelProfile {
            thinking: ThinkingPolicy::Off,
            ..ModelProfile::default()
        };
        let err = bare.request_extra_body(false).unwrap_err();
        assert!(err.contains("thinking_control"), "{err}");
        assert!(toml::from_str::<Config>("[model_profile.x]\nthinking = \"maybe\"\n").is_err());
    }

    #[test]
    fn unknown_reasoning_field_warns_with_the_accepted_names() {
        let mut profile = ModelProfile::default();
        assert!(profile.reasoning_field_warning().is_none());
        for known in REASONING_FIELDS {
            profile.reasoning_field = Some(known.into());
            assert!(profile.reasoning_field_warning().is_none(), "{known}");
        }
        profile.reasoning_field = Some("thoughts".into());
        let warning = profile.reasoning_field_warning().expect("warning");
        assert!(warning.contains("\"thoughts\""), "{warning}");
        for known in REASONING_FIELDS {
            assert!(warning.contains(known), "{warning}");
        }
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
    fn hooks_load_from_config_and_validate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[hooks]\ntimeout_secs = 7\n\n[hooks.guard]\nevent = \"pre_tool\"\ncommand = [\"/bin/true\"]\n",
        )
        .unwrap();
        let cfg = Config::load(&path, None).unwrap();
        assert_eq!(cfg.hooks.timeout_secs, 7);
        assert_eq!(cfg.hooks.max_output_bytes, 16 * 1024);
        assert_eq!(
            cfg.hooks.entries["guard"].event,
            crate::config::HookEvent::PreTool
        );
        assert_eq!(
            cfg.hooks.entries["guard"].on_error(),
            crate::config::HookOnError::Block
        );

        std::fs::write(&path, "[hooks.broken]\nevent = \"stop\"\ncommand = []\n").unwrap();
        let error = Config::load(&path, None).unwrap_err().to_string();
        assert!(error.contains("hook"), "{error}");
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
    fn family_budgets_accept_zero_and_reject_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[budgets]\nmax_same_tool_family = 0\nmax_same_tool_family_success = 0\n",
        )
        .unwrap();
        let cfg = Config::load(&path, None).unwrap();
        assert_eq!(cfg.budgets.max_same_tool_family, 0, "0 disables the guard");
        assert_eq!(cfg.budgets.max_same_tool_family_success, 0);

        std::fs::write(&path, "[budgets]\nmax_same_tool_family_success = 1\n").unwrap();
        let error = Config::load(&path, None).unwrap_err().to_string();
        assert!(error.contains("max_same_tool_family_success"), "{error}");
        assert!(error.contains("0 (disabled) or at least 2"), "{error}");

        std::fs::write(&path, "[budgets]\nmax_same_tool_family = 1\n").unwrap();
        let error = Config::load(&path, None).unwrap_err().to_string();
        assert!(error.contains("max_same_tool_family must be"), "{error}");
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

    #[test]
    fn save_setting_preserves_unrelated_keys_and_the_file_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "default_profile = \"full\"\n\n[model]\napi_key = \"secret\"\n\n[ui]\ntheme = \"light\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        Config::save_setting(&path, "ui", "show_reasoning", toml::Value::Boolean(true)).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Value = toml::from_str(&raw).unwrap();
        assert_eq!(parsed["ui"]["show_reasoning"].as_bool(), Some(true));
        assert_eq!(parsed["ui"]["theme"].as_str(), Some("light"));
        assert_eq!(parsed["model"]["api_key"].as_str(), Some("secret"));
        assert_eq!(parsed["default_profile"].as_str(), Some("full"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the existing private mode is kept");
        }
    }

    #[test]
    fn save_setting_creates_a_private_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/config.toml");

        Config::save_ui_setting(&path, "theme", toml::Value::String("hati".into())).unwrap();

        assert!(path.is_file());
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Value = toml::from_str(&raw).unwrap();
        assert_eq!(parsed["ui"]["theme"].as_str(), Some("hati"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "a new config must be private");
        }
    }

    #[test]
    fn default_profile_persists_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[ui]\ntheme = \"light\"\n").unwrap();

        Config::save_default_profile(&path, "full").unwrap();

        let cfg = Config::load(&path, None).unwrap();
        assert_eq!(cfg.default_profile, "full");
        assert_eq!(cfg.profile_name(), "full");
        assert_eq!(cfg.ui.theme, "light", "unrelated keys survive the write");
    }

    #[test]
    fn show_reasoning_persists_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "default_profile = \"coding\"\n").unwrap();

        Config::save_ui_setting(&path, "show_reasoning", toml::Value::Boolean(true)).unwrap();

        let cfg = Config::load(&path, None).unwrap();
        assert!(cfg.ui.show_reasoning);
        assert_eq!(cfg.default_profile, "coding");
    }
}
