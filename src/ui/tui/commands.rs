//! Slash commands: `/model`, `/login`, `/theme`, `/resume`, `/workflow`,
//! `/sec`, and the rest. Each `command_*` method runs one command.

use super::*;

impl App {
    pub(super) async fn run_command(&mut self, text: &str) {
        let body = text.trim().trim_start_matches('/');
        let (name, rest) = match body.find(char::is_whitespace) {
            Some(position) => (&body[..position], body[position..].trim()),
            None => (body, ""),
        };
        let always = matches!(name, "help" | "quit" | "debug" | "trust" | "approve");
        if self.turn_active && !always {
            self.set_notice(
                "cancel the running turn first (Esc) before using this command",
                Role::Warning,
            );
            return;
        }

        if self.switching.is_some() && matches!(name, "model" | "login" | "profile") {
            self.set_notice(
                "a switch is still loading; try again when it finishes",
                Role::Warning,
            );
            return;
        }

        match name {
            "help" => self.command_help(rest),
            "quit" => self.quit = true,
            "model" => self.command_model(rest).await,
            "login" => self.command_login(rest).await,
            "profile" => self.command_profile(rest).await,
            "theme" => self.command_theme(rest),
            "tree" => self.command_tree(),
            "rewind" => self.command_rewind(rest),
            "fork" => self.command_fork(),
            "new" => self.command_new().await,
            "resume" => self.command_resume(rest).await,
            "sessions" => self.command_sessions(),
            "compact" => self.command_compact(),
            "commit" => self.command_commit(rest).await,
            "pr" => self.command_pr(rest).await,
            "plan" => self.command_plan(rest).await,
            "permissions" => self.command_permissions(rest).await,
            "tools" => self.command_tools(),
            "mcp" => self.command_mcp(),
            "skill" => self.command_skill(rest),
            "workflow" => self.command_workflow(rest).await,
            "sec" => self.command_sec(rest),
            "approve" => self.command_approve(),
            "trust" => self.command_trust(rest).await,
            "verify" => self.command_verify(rest).await,
            "export" => self.command_export(rest),
            "clear" => self.command_clear(),
            "debug" => {
                self.debug = !self.debug;
                self.set_notice(
                    if self.debug {
                        "debug panel on"
                    } else {
                        "debug panel off"
                    },
                    Role::Muted,
                );
            }
            other => self.unknown_command(other),
        }
        self.refresh_completion_sources(matches!(name, "new" | "resume" | "sessions"));
    }

    pub(super) fn command_help(&mut self, topic: &str) {
        let topic = topic.trim().trim_start_matches('/');
        if topic.is_empty() {
            self.transcript.push_note("help", HELP, false);
            return;
        }
        match COMMANDS.iter().find(|(name, _)| *name == topic) {
            Some((name, description)) => {
                let usage = match command_usage(name) {
                    "" => format!("/{name}"),
                    usage => usage.to_string(),
                };
                self.transcript.push_note(
                    "help",
                    format!("/{name} — {description}\nusage: {usage}"),
                    false,
                );
            }
            None => self.unknown_command(topic),
        }
    }

    /// Open a picker modal, or say there is nothing to pick.
    pub(super) fn open_picker(&mut self, picker: PickerModal, empty: &str) {
        if picker.items.is_empty() {
            self.set_notice(empty.to_string(), Role::Muted);
            return;
        }
        let mut modal = ModalData::Picker(picker);
        modal.ensure_visible(15);
        self.modal = Some(modal);
    }

    pub(super) fn unknown_command(&mut self, name: &str) {
        let available: Vec<String> = COMMANDS
            .iter()
            .map(|(command, _)| format!("/{command}"))
            .collect();
        self.transcript.push_note(
            "unknown command",
            format!(
                "/{name} is not a command.\nAvailable: {}",
                available.join(" ")
            ),
            true,
        );
        self.set_notice(format!("unknown command /{name}; try /help"), Role::Error);
    }

    pub(super) async fn command_model(&mut self, name: &str) {
        let name = name.trim();
        if !name.is_empty() {
            let mut parts = name.split_whitespace();
            let model = parts.next().unwrap_or_default();
            let preset = parts.next();
            if parts.next().is_some() {
                self.set_notice("usage: /model <name> [model-profile]", Role::Error);
                return;
            }
            if let Some(preset) = preset {
                if !self.config.model_profiles.contains_key(preset) {
                    let available: Vec<&str> = self
                        .config
                        .model_profiles
                        .keys()
                        .map(String::as_str)
                        .collect();
                    self.set_notice(
                        format!(
                            "unknown model profile {preset:?}; available: {}",
                            available.join(", ")
                        ),
                        Role::Error,
                    );
                    return;
                }
                self.switch_model_with(model, Some(preset)).await;
            } else {
                self.switch_model(model).await;
            }
            return;
        }
        let Some(core) = self.core().cloned() else {
            return;
        };
        let current = if core.logged_in() {
            core.provider.model().to_string()
        } else {
            String::new()
        };
        let endpoint = core.config.model.endpoint.clone();
        // A warm list (probed at startup or by the last picker) opens the
        // picker at once; a refresh lands in the open picker.
        if !self.completion_sources.models.is_empty() {
            let picker = PickerModal::new(
                "model",
                "Models",
                model_items(&self.completion_sources.models),
                current,
            )
            .with_subtitle(endpoint);
            self.open_picker(picker, "the endpoint reports no models");
            self.spawn_models_refresh();
            return;
        }
        match probe_endpoint_models(&core.config.model).await {
            Ok(models) if !models.is_empty() => {
                self.completion_sources.models = models.clone();
                self.completion_cache.replace(None);
                let picker = PickerModal::new("model", "Models", model_items(&models), current)
                    .with_subtitle(endpoint);
                self.open_picker(picker, "the endpoint reports no models");
            }
            result => {
                let mut lines = vec![
                    format!("endpoint: {endpoint}"),
                    format!(
                        "model: {}",
                        if current.is_empty() {
                            "(not set)"
                        } else {
                            current.as_str()
                        }
                    ),
                ];
                match result {
                    Ok(_) => {
                        lines.push("available models: (the endpoint reports none)".into());
                        lines.push("load a model, then run /model again".into());
                    }
                    Err(error) => {
                        lines.push(format!("cannot list models: {error}"));
                        lines.push(
                            "start the server, or run /login <endpoint> [model] [api-key]".into(),
                        );
                    }
                }
                self.transcript.push_note("models", lines.join("\n"), false);
            }
        }
    }

    /// Switch the active model for this session; the saved config is
    /// untouched. The rebuild (which may wait for the server to load the
    /// model) runs in the background.
    pub(super) async fn switch_model(&mut self, name: &str) {
        self.switch_model_with(name, None).await;
    }

    /// Switch model and optionally pin a sampler preset (`/model <name>
    /// <model-profile>`). Auto matching still applies when no preset is given.
    pub(super) async fn switch_model_with(&mut self, name: &str, preset: Option<&str>) {
        if self
            .core()
            .is_some_and(|core| core.logged_in() && core.provider.model() == name)
            && preset.is_none()
        {
            self.set_notice(format!("model: {name} (unchanged)"), Role::Muted);
            return;
        }
        let mut config = self.config.clone();
        config.model.model = name.to_string();
        if let Some(preset) = preset {
            config.model_profile_override = Some(preset.to_string());
        }
        self.spawn_rebuild(Switch::Model(name.to_string()), config);
    }

    pub(super) async fn command_login(&mut self, rest: &str) {
        let Some(core) = self.core().cloned() else {
            return;
        };
        let mut parts = rest.split_whitespace();
        let Some(endpoint) = parts.next() else {
            self.transcript
                .push_note("login", login_status(&core), false);
            return;
        };
        let model = parts.next();
        let api_key = parts.next();
        if parts.next().is_some() {
            self.set_notice("usage: /login <endpoint> [model] [api-key]", Role::Error);
            return;
        }
        let endpoint = normalize_endpoint(endpoint);
        let mut candidate = self.config.clone();
        let endpoint_changed = crate::provider::endpoint_urls(&candidate.model.endpoint).api
            != crate::provider::endpoint_urls(&endpoint).api;
        candidate.model.endpoint = endpoint.clone();
        if endpoint_changed && model.is_none() {
            candidate.model.model.clear();
        }
        if endpoint_changed && api_key.is_none() {
            candidate.model.api_key.clear();
        }
        if let Some(model) = model {
            candidate.model.model = model.to_string();
        }
        if let Some(api_key) = api_key {
            candidate.model.api_key = if api_key == "-" {
                String::new()
            } else {
                api_key.to_string()
            };
        }

        let models = match probe_endpoint_models(&candidate.model).await {
            Ok(models) => {
                self.completion_sources.models = models.clone();
                models
            }
            Err(error) => {
                self.transcript.push_note(
                    "login",
                    format!(
                        "cannot log in to {endpoint}: {error}\n\nStart the inference server and retry, or check the endpoint URL."
                    ),
                    true,
                );
                self.set_notice(format!("login failed: {error}"), Role::Error);
                return;
            }
        };

        if candidate.model.model.trim().is_empty() {
            match models.as_slice() {
                [only] => candidate.model.model = only.clone(),
                [] => {
                    self.transcript.push_note(
                        "login",
                        format!(
                            "{endpoint} reports no models.\nLoad a model, or pass its id: /login <endpoint> <model>"
                        ),
                        true,
                    );
                    self.set_notice("login failed: endpoint reports no models", Role::Error);
                    return;
                }
                _ => {
                    self.transcript.push_note(
                        "login",
                        format!(
                            "{endpoint} exposes multiple models:\n{}\n\nChoose one: /login <endpoint> <model>",
                            models.join("\n")
                        ),
                        true,
                    );
                    self.set_notice("login failed: choose a model", Role::Error);
                    return;
                }
            }
        } else if !models.is_empty()
            && !models
                .iter()
                .any(|available| available == candidate.model.model.trim())
        {
            self.transcript.push_note(
                "login",
                format!(
                    "{endpoint} has no model {:?}. Available:\n{}\n\nRetry with: /login <endpoint> <model>",
                    candidate.model.model,
                    models.join("\n")
                ),
                true,
            );
            self.set_notice("login failed: model not found", Role::Error);
            return;
        }

        let previous = self.config.clone();
        if let Err(error) = self.rebuild_core_with(candidate).await {
            self.config = previous;
            self.set_notice(format!("login failed: {error}"), Role::Error);
            return;
        }
        let model = self.config.model.model.clone();
        let key_state = if self.config.model.api_key.is_empty() {
            "not set"
        } else {
            "set"
        };
        match self.persist_connection() {
            Ok(path) => {
                self.transcript.push_note(
                    "login",
                    format!(
                        "endpoint: {endpoint}\nmodel: {model}\napi key: {key_state}\nsaved: {}",
                        path.display()
                    ),
                    false,
                );
                self.set_notice(format!("logged in · {model} · saved"), Role::Success);
            }
            Err(error) => {
                self.transcript.push_note(
                    "login",
                    format!("logged in as {model}, but settings were not saved: {error}"),
                    true,
                );
                self.set_notice(
                    format!("logged in · settings not saved: {error}"),
                    Role::Warning,
                );
            }
        }
    }

    pub(super) fn persist_connection(&self) -> Result<PathBuf> {
        let core = self.core().context("no runtime")?;
        let path = core.paths.config_file();
        core.config.model.save_connection(&path)?;
        Ok(path)
    }

    pub(super) async fn command_profile(&mut self, name: &str) {
        if name.is_empty() {
            let items = self
                .config
                .profiles
                .iter()
                .map(|(profile, data)| {
                    PickItem::run(
                        "profile",
                        profile.clone(),
                        data.description.clone().unwrap_or_default(),
                    )
                })
                .collect();
            let picker = PickerModal::new(
                "profile",
                "Profiles",
                items,
                self.config.profile_name().to_string(),
            );
            self.open_picker(picker, "no profiles are configured");
            return;
        }
        if !self.config.profiles.contains_key(name) {
            let available: Vec<String> = self.config.profiles.keys().cloned().collect();
            let message = format!(
                "unknown profile {name:?}; available: {}",
                available.join(", ")
            );
            self.set_notice(message, Role::Error);
            return;
        }
        if self.config.profile_name() == name {
            self.set_notice(format!("profile: {name} (unchanged)"), Role::Muted);
            return;
        }
        let mut config = self.config.clone();
        config.profile_name = name.to_string();
        self.spawn_rebuild(Switch::Profile(name.to_string()), config);
    }

    /// `/plan`: enter plan mode, or show the plan; `/plan approve` restores
    /// the previous mode and tool set; `/plan exit` discards the plan.
    pub(super) async fn command_plan(&mut self, rest: &str) {
        let Some(core) = self.core().cloned() else {
            return;
        };
        let permissions = core.permissions.clone();
        let in_plan = permissions.mode() == crate::policy::PermissionMode::Plan;
        let (status, notice) = match rest.trim() {
            "" if in_plan => {
                let plan = core
                    .session
                    .lock()
                    .ok()
                    .and_then(|session| crate::tools::todo::latest_plan(&session));
                let text = match plan {
                    Some((status, tasks)) if !tasks.is_empty() => {
                        let mut text = format!("{status}:");
                        for (index, task) in tasks.iter().enumerate() {
                            text.push_str(&format!("\n{}. {task}", index + 1));
                        }
                        text.push_str("\n\n/plan approve applies it · /plan exit discards it");
                        text
                    }
                    _ => "plan mode is active; no plan yet. Describe the task; the model must answer with a todo plan.".into(),
                };
                self.transcript.push_note("plan", text, false);
                return;
            }
            "" => {
                permissions.enter_plan();
                (
                    None,
                    "plan mode: read-only tools; the next turn must produce a todo plan",
                )
            }
            "approve" | "exit" if !in_plan => {
                self.set_notice("plan mode is not active; /plan enters it", Role::Muted);
                return;
            }
            "approve" => {
                permissions.leave_plan();
                (
                    Some("approved"),
                    "plan approved: the previous mode and tools are back",
                )
            }
            "exit" => {
                permissions.leave_plan();
                (
                    Some("discarded"),
                    "plan discarded: the previous mode and tools are back",
                )
            }
            _ => {
                self.set_notice(command_usage("plan"), Role::Error);
                return;
            }
        };
        let mut config = self.config.clone();
        config.permissions.mode = permissions.mode();
        if let Err(error) = self.rebuild_core_with(config).await {
            // Keep the mode and tool set consistent: undo the switch.
            if status.is_some() {
                permissions.enter_plan();
            } else {
                permissions.leave_plan();
            }
            self.set_notice(format!("/plan failed: {error}"), Role::Error);
            return;
        }
        if let Ok(mut session) = core.session.lock() {
            let entry = match status {
                Some(status) => crate::tools::todo::plan_entry(&session, status),
                None => crate::session::CustomData::new("permission-mode")
                    .with("mode", "plan")
                    .with("source", "/plan"),
            };
            if let Err(error) = session.append(EntryKind::Custom(entry)) {
                tracing::warn!(%error, "cannot record the plan-mode change");
            }
        }
        self.set_notice(notice, Role::Success);
    }

    /// `/permissions`: pick or set what runs without asking. Levels are
    /// `normal`, `accept-edits`, `deny-all`, and `yolo`; plan mode keeps its
    /// own command.
    pub(super) async fn command_permissions(&mut self, rest: &str) {
        use crate::policy::PermissionMode;

        let argument = rest.trim();
        if argument.is_empty() {
            let Some(core) = self.core().cloned() else {
                return;
            };
            let current = core.permissions.mode();
            let items = PermissionMode::LEVELS
                .iter()
                .map(|mode| PickItem::run("permissions", mode.label(), mode.description()))
                .collect();
            let picker = PickerModal::new(
                "permissions",
                "Permissions · what runs without asking",
                items,
                current.label(),
            );
            self.open_picker(picker, "no permission levels");
            return;
        }
        let Some(mode) = PermissionMode::parse(argument) else {
            self.set_notice(
                "permissions: normal, accept-edits, deny-all, or yolo",
                Role::Error,
            );
            return;
        };
        if mode == PermissionMode::Plan {
            self.set_notice("plan mode has its own command: /plan", Role::Muted);
            return;
        }
        let already = self
            .core()
            .is_some_and(|core| core.permissions.mode() == mode);
        if mode == PermissionMode::Yolo && !already {
            self.pending_mode_confirm = Some(PermissionMode::Yolo);
            self.set_notice(
                "yolo allows every call without asking (deny rules still apply): press y to confirm, any other key cancels",
                Role::Warning,
            );
            return;
        }
        self.set_permission_mode(mode).await;
    }

    /// Switch the session's permission level. The config file is never
    /// touched, and `yolo` is recorded in the session like `--yolo`.
    pub(super) async fn set_permission_mode(&mut self, mode: crate::policy::PermissionMode) {
        let Some(core) = self.core().cloned() else {
            return;
        };
        let permissions = core.permissions.clone();
        let previous = permissions.mode();
        if previous == mode {
            self.set_notice(
                format!("permissions: {} (unchanged)", mode.label()),
                Role::Muted,
            );
            return;
        }
        let left_plan = previous == crate::policy::PermissionMode::Plan;
        let plan_fallback = if left_plan {
            permissions.leave_plan()
        } else {
            None
        };
        permissions.set_mode(mode);
        if left_plan {
            // Plan mode filtered the tool set at rebuild; bring it back.
            if let Err(error) = self.rebuild_core_with(self.config.clone()).await {
                permissions.set_mode(plan_fallback.unwrap_or_default());
                permissions.enter_plan();
                self.set_notice(format!("/permissions failed: {error}"), Role::Error);
                return;
            }
        }
        if let Ok(mut session) = core.session.lock() {
            let entry = crate::session::CustomData::new("permission-mode")
                .with("mode", mode.label())
                .with("source", "/permissions");
            if let Err(error) = session.append(EntryKind::Custom(entry)) {
                tracing::warn!(%error, "cannot record the permission-mode change");
            }
        }
        if mode == crate::policy::PermissionMode::Yolo {
            self.transcript.push_note(
                "session warning",
                "yolo: every tool call runs without asking; only deny rules still apply",
                false,
            );
        }
        self.set_notice(
            format!("permissions: {} — {}", mode.label(), mode.description()),
            if mode == crate::policy::PermissionMode::Yolo {
                Role::Warning
            } else {
                Role::Success
            },
        );
    }

    pub(super) async fn rebuild_core_with(&mut self, config: Config) -> Result<()> {
        let Some(old) = self.core().cloned() else {
            bail!("no runtime");
        };
        let core = app::rebuild_core(&old, config).await?;
        self.apply_core(core);
        Ok(())
    }

    /// Install a rebuilt core and refresh everything derived from it.
    pub(super) fn apply_core(&mut self, core: Arc<Core>) {
        if let Some(runtime) = &mut self.runtime {
            runtime.core = core.clone();
        }
        self.config = core.config.clone();
        self.header.model = core.provider.model().to_string();
        self.header.logged_in = core.logged_in();
        self.header.profile = self.config.profile_name().to_string();
        self.header.tool_mode = core.tool_mode_label().unwrap_or_default();
        self.header.trust = self.config.trust || self.options.trust;
        self.window = core.effective_window().0;
        self.header.window = self.window;
        self.used_tokens = estimate_used(&core);
        self.header.used = self.used_tokens;
        self.sync_banner();
    }

    pub(super) fn apply_theme_named(&mut self, name: &str) {
        let Some(paths) = self.core().map(|core| core.paths.clone()) else {
            return;
        };
        self.theme = Theme::load(
            name,
            &paths.themes_dir(),
            self.config.ui.color_mode,
            self.no_color,
        );
        self.config.ui.theme = name.to_string();
        self.input.apply_theme(&self.theme);
    }

    /// Undo a `/theme` picker's live preview when it closes without a pick.
    pub(super) fn restore_previewed_theme(&mut self) {
        if let Some(name) = self.theme_before_preview.take()
            && name != self.theme.name
        {
            self.apply_theme_named(&name);
        }
    }

    pub(super) fn command_theme(&mut self, argument: &str) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let paths = runtime.core.paths.clone();
        match argument {
            "" => {
                let current = self.theme.name.clone();
                let mut items = vec![PickItem::run(
                    "theme",
                    "save",
                    format!("write {current} to the config"),
                )];
                items.extend(
                    available_theme_names(&paths)
                        .into_iter()
                        .map(|name| PickItem::run("theme", name, "")),
                );
                self.theme_before_preview = Some(current.clone());
                let picker = PickerModal::new("theme", "Themes · live preview", items, current);
                self.open_picker(picker, "no themes found");
            }
            "save" => {
                let path = paths.config_file();
                match save_theme(&path, &self.theme.name) {
                    Ok(()) => {
                        self.set_notice(
                            format!("theme saved to {}", path.display()),
                            Role::Success,
                        );
                    }
                    Err(error) => {
                        self.set_notice(format!("cannot save theme: {error}"), Role::Error);
                    }
                }
            }
            name => {
                let names = available_theme_names(&paths);
                if !names.iter().any(|candidate| candidate == name) {
                    self.set_notice(
                        format!("unknown theme {name:?}; available: {}", names.join(", ")),
                        Role::Error,
                    );
                    return;
                }
                self.apply_theme_named(name);
                self.set_notice(format!("theme: {name}"), Role::Success);
            }
        }
    }

    pub(super) fn command_tree(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let view = match runtime.session.lock() {
            Ok(session) => TreeView::new(&session),
            Err(poison) => TreeView::new(&poison.into_inner()),
        };
        if view.is_empty() {
            self.set_notice("the session tree is empty", Role::Muted);
            return;
        }
        self.modal = Some(ModalData::Tree(view));
    }

    pub(super) fn command_rewind(&mut self, argument: &str) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let core = runtime.core.clone();
        let session_handle = runtime.session.clone();
        let mut turns = 1usize;
        let mut force = false;
        for word in argument.split_whitespace() {
            if word.eq_ignore_ascii_case("force") || word == "--force" {
                force = true;
            } else if let Ok(turns_parsed) = word.parse::<usize>() {
                turns = turns_parsed;
            } else {
                self.set_notice(
                    format!("usage: /rewind [turns] [force] (got {word:?})"),
                    Role::Error,
                );
                return;
            }
        }
        let mut session = match session_handle.lock() {
            Ok(session) => session,
            Err(poison) => poison.into_inner(),
        };
        let target = match crate::session::rewind::target_before_turn(&session, turns) {
            Ok(target) => target,
            Err(message) => {
                drop(session);
                self.set_notice(message.clone(), Role::Warning);
                self.transcript.push_note("rewind", message, true);
                return;
            }
        };
        let plan = match crate::session::rewind::plan(&session, &target) {
            Ok(plan) => plan,
            Err(error) => {
                drop(session);
                let message = format!("{error:#}");
                self.set_notice(message.clone(), Role::Error);
                self.transcript.push_note("rewind", message, true);
                return;
            }
        };
        let paths: Vec<_> = plan
            .files
            .iter()
            .filter(|file| file.content.is_some())
            .map(|file| file.path.clone())
            .collect();
        let report = match crate::session::rewind::apply(&mut session, &plan, force) {
            Ok(report) => report,
            Err(error) => {
                drop(session);
                let message = format!("{error:#}");
                self.set_notice(
                    message.lines().next().unwrap_or(&message).to_string(),
                    Role::Error,
                );
                self.transcript.push_note("rewind", message, true);
                return;
            }
        };
        let _ = session.fsync();
        drop(session);
        // Restored bytes are what the harness itself wrote now; the model does
        // not have to re-read them before the next edit.
        let ctx = core.tool_context();
        for path in &paths {
            crate::tools::write::record_own_write(&ctx, path);
        }
        self.refresh_session_blocks();
        let message = format!("rewound to {}: {}", report.target, report.summary());
        self.transcript.push_note("rewind", message.clone(), false);
        self.set_notice(message, Role::Success);
    }

    pub(super) fn command_fork(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let target = match runtime.session.lock() {
            Ok(session) => session
                .branch_entries()
                .into_iter()
                .rev()
                .find(|entry| entry.is_assistant())
                .map(|entry| entry.id.clone()),
            Err(poison) => poison
                .into_inner()
                .branch_entries()
                .into_iter()
                .rev()
                .find(|entry| entry.is_assistant())
                .map(|entry| entry.id.clone()),
        };
        let Some(target) = target else {
            self.set_notice("no assistant step to fork from", Role::Warning);
            return;
        };
        match self.checkout_target(&target, "fork") {
            Ok(()) => {
                self.set_notice("forked from the latest assistant step", Role::Success);
            }
            Err(error) => self.set_notice(error, Role::Error),
        }
    }

    pub(super) async fn command_new(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let empty = match runtime.session.lock() {
            Ok(session) => session.tree.entries().len() <= 1,
            Err(poison) => poison.into_inner().tree.entries().len() <= 1,
        };
        if empty {
            self.set_notice("already in a fresh session", Role::Muted);
            return;
        }
        self.open_session(None).await;
    }

    pub(super) async fn command_resume(&mut self, argument: &str) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let current = match runtime.session.lock() {
            Ok(session) => session.path.clone(),
            Err(poison) => poison.into_inner().path.clone(),
        };
        let Some(current) = current else {
            self.set_notice("sessions are disabled (--no-session)", Role::Warning);
            return;
        };
        let items: Vec<SessionItem> = scan_sessions(&runtime.core.paths)
            .into_iter()
            .filter(|item| item.path != current)
            .collect();
        if items.is_empty() {
            self.set_notice("no other sessions to resume for this project", Role::Muted);
            return;
        }
        let argument = argument.trim();
        if argument.is_empty() {
            self.modal = Some(ModalData::Sessions(SessionsModal {
                items,
                selected: 0,
                scroll: 0,
            }));
            return;
        }
        let path = if argument == "last" {
            Ok(items[0].path.clone())
        } else {
            match_session(&items, argument)
        };
        match path {
            Ok(path) => self.switch_session(path).await,
            Err(message) => {
                self.transcript.push_note("resume", message, true);
                self.set_notice(format!("no session matches {argument:?}"), Role::Error);
            }
        }
    }

    pub(super) fn command_sessions(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let ephemeral = match runtime.session.lock() {
            Ok(session) => session.path.is_none(),
            Err(poison) => poison.into_inner().path.is_none(),
        };
        if ephemeral {
            self.set_notice("sessions are disabled (--no-session)", Role::Warning);
            return;
        }
        let items = scan_sessions(&runtime.core.paths);
        if items.is_empty() {
            self.set_notice("no sessions for this project", Role::Muted);
            return;
        }
        self.modal = Some(ModalData::Sessions(SessionsModal {
            items,
            selected: 0,
            scroll: 0,
        }));
    }

    pub(super) fn command_compact(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let findings = runtime.core.findings_summary();
        let result = {
            let mut session = match runtime.session.lock() {
                Ok(session) => session,
                Err(poison) => poison.into_inner(),
            };
            let budget = context::compact::summary_budget(&runtime.core.budget);
            let summary = context::build_summary(&session, findings.as_ref(), budget);
            let result = session
                .append(EntryKind::Compaction(CompactionData {
                    summary: summary.summary.clone(),
                    tokens_before: summary.tokens_before,
                    reason: "manual".into(),
                    thread_id: None,
                    keep_from: None,
                }))
                .map(|_| summary.tokens_before);
            let _ = session.fsync();
            result
        };
        match result {
            Ok(tokens) => {
                self.refresh_session_blocks();
                self.set_notice(format!("compacted ({tokens} tokens before)"), Role::Success);
            }
            Err(error) => self.set_notice(format!("compaction failed: {error}"), Role::Error),
        }
    }

    pub(super) fn command_tools(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let items: Vec<ToolItem> = runtime
            .core
            .tools
            .iter()
            .map(|tool| ToolItem {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
            })
            .collect();
        if items.is_empty() {
            self.set_notice("no tools are active in this profile", Role::Warning);
            return;
        }
        self.modal = Some(ModalData::Tools(ToolsModal {
            items,
            selected: 0,
            scroll: 0,
        }));
    }

    /// Approve exactly the next sensitive action after a provenance warning.
    pub(super) fn command_approve(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let pending = {
            let mut state = runtime
                .core
                .provenance
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            state.approve()
        };
        match pending {
            Some(pending) => self.set_notice(
                format!(
                    "approved the next sensitive action; warning from {} ({})",
                    pending.origin,
                    pending.patterns.join(", ")
                ),
                Role::Success,
            ),
            None => self.set_notice(
                "no provenance warning is pending; nothing to approve",
                Role::Muted,
            ),
        }
    }

    pub(super) fn command_mcp(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let statuses = runtime.core.mcp.status();
        if statuses.is_empty() {
            self.transcript.push_note(
                "mcp",
                "no MCP servers configured; add an [mcp.<name>] table to config.toml (see docs/mcp.md)",
                false,
            );
            return;
        }
        let lines: Vec<String> = statuses.iter().map(|status| status.summary()).collect();
        let failed = statuses
            .iter()
            .any(|status| matches!(status.state, crate::mcp::ServerState::Failed(_)));
        self.transcript.push_note("mcp", lines.join("\n"), failed);
    }

    pub(super) fn command_skill(&mut self, name: &str) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let skills = SkillIndex::load(&runtime.core.paths, self.config.trust);
        let advertised = skills.advertised_for(&crate::tools::active_packs(&runtime.core.tools));
        if name.is_empty() {
            let items = skill_choices(&runtime.core.paths, self.config.trust, &runtime.core.tools)
                .into_iter()
                .map(|(name, description)| PickItem::run("skill", name, description))
                .collect();
            let picker = PickerModal::new("skill", "Skills · appends instructions", items, "");
            self.open_picker(picker, "no skills are available in this profile");
            return;
        }
        let Some(skill) = skills.get(name) else {
            self.set_notice(
                format!(
                    "unknown skill {name:?}; available: {}",
                    advertised.names().join(", ")
                ),
                Role::Error,
            );
            return;
        };
        let content = format!(
            "{}\n\n{}",
            crate::tools::skill::skill_marker(&skill.name),
            skill.full_text()
        );
        let result = {
            let mut session = match runtime.session.lock() {
                Ok(session) => session,
                Err(poison) => poison.into_inner(),
            };
            let result = session
                .append(EntryKind::User(UserData {
                    attachments: Vec::new(),
                    content,
                }))
                .map(|entry| entry.id);
            let _ = session.fsync();
            result
        };
        match result {
            Ok(entry) => {
                self.refresh_session_blocks();
                self.set_notice(format!("skill {name} added as {entry}"), Role::Success);
            }
            Err(error) => self.set_notice(format!("cannot add skill: {error}"), Role::Error),
        }
    }

    pub(super) async fn command_workflow(&mut self, name: &str) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let paths = runtime.core.paths.clone();
        let trust = self.config.trust;
        if name.is_empty() {
            let items = available_workflows(&paths, trust)
                .into_iter()
                .map(|name| {
                    let description = workflow_description(&name);
                    PickItem::run("workflow", name, description)
                })
                .collect();
            let picker = PickerModal::new("workflow", "Workflows · starts a branch", items, "");
            let empty = format!("no workflows in {}", paths.workflows_dir().display());
            self.open_picker(picker, &empty);
            return;
        }
        let global = paths.workflows_dir().join(format!("{name}.toml"));
        let project = paths
            .project_dir()
            .join("workflows")
            .join(format!("{name}.toml"));
        let workflow: WorkflowSpec = if global.is_file() {
            match load_workflow_file(&global) {
                Ok(workflow) => workflow,
                Err(message) => {
                    self.set_notice(message, Role::Error);
                    return;
                }
            }
        } else if project.is_file() {
            if !trust {
                self.set_notice(
                    "project workflows require --trust; restart with --trust to load them",
                    Role::Error,
                );
                return;
            }
            match load_workflow_file(&project) {
                Ok(workflow) => workflow,
                Err(message) => {
                    self.set_notice(message, Role::Error);
                    return;
                }
            }
        } else if let Some(bundled) = workflows::get(name) {
            bundled.clone()
        } else {
            let names = available_workflows(&paths, trust);
            self.set_notice(
                format!(
                    "unknown workflow {name:?}; available: {}",
                    if names.is_empty() {
                        "none".to_string()
                    } else {
                        names.join(", ")
                    }
                ),
                Role::Error,
            );
            return;
        };
        let skills = SkillIndex::load(&paths, trust);
        if !workflow.steps.is_empty() {
            self.set_notice(
                format!(
                    "workflow {name} has deterministic steps; run it from the shell with `tot workflow run {name}`"
                ),
                Role::Error,
            );
            return;
        }
        for skill in &workflow.skills {
            if skills.get(skill).is_none() {
                self.set_notice(
                    format!(
                        "workflow {name} uses unknown skill {skill:?}; available: {}",
                        skills.names().join(", ")
                    ),
                    Role::Error,
                );
                return;
            }
        }
        let mut config = self.config.clone();
        if let Some(profile) = &workflow.profile {
            if !config.profiles.contains_key(profile) {
                let available: Vec<&str> = config.profiles.keys().map(String::as_str).collect();
                self.set_notice(
                    format!(
                        "workflow {name} uses unknown profile {profile:?}; available: {}",
                        available.join(", ")
                    ),
                    Role::Error,
                );
                return;
            }
            config.profile_name = profile.clone();
        }
        if !workflow.tools.is_empty() {
            let base = config.profile().clone();
            let derived_name = format!("workflow-{}", crate::text::slug(name, '-'));
            let mut derived = base;
            // A workflow's tool list is an allowlist: drop the base profile's
            // packs so unlisted tools in those packs are not silently enabled.
            derived.packs.clear();
            derived.tools = workflow.tools.clone();
            derived.description = workflow
                .description
                .clone()
                .or_else(|| Some(workflow.name.clone()));
            config.profiles.insert(derived_name.clone(), derived);
            config.profile_name = derived_name;
        }
        let previous = self.config.clone();
        if let Err(error) = self.rebuild_core_with(config).await {
            self.config = previous;
            self.set_notice(format!("workflow profile failed: {error}"), Role::Error);
            return;
        }
        let result = {
            let Some(runtime) = &self.runtime else {
                return;
            };
            let mut session = match runtime.session.lock() {
                Ok(session) => session,
                Err(poison) => poison.into_inner(),
            };
            let mut result = Ok(());
            if let Some(leaf) = session.active_leaf_id() {
                match session.tree.resolve_checkout(&leaf) {
                    Ok(target) => {
                        if let Err(error) = session.checkout(&target, &format!("workflow {name}")) {
                            result = Err(error);
                        }
                    }
                    Err(error) => result = Err(anyhow!("{error}")),
                }
            }
            if result.is_ok()
                && let Err(error) = session.append(EntryKind::User(UserData {
                    attachments: Vec::new(),
                    content: workflow.prompt.clone(),
                }))
            {
                result = Err(error);
            }
            if result.is_ok() {
                for skill_name in &workflow.skills {
                    if let Some(skill) = skills.get(skill_name) {
                        let content = format!(
                            "{}\n\n{}",
                            crate::tools::skill::skill_marker(&skill.name),
                            skill.full_text()
                        );
                        if let Err(error) = session.append(EntryKind::User(UserData {
                            attachments: Vec::new(),
                            content,
                        })) {
                            result = Err(error);
                            break;
                        }
                    }
                }
            }
            let _ = session.fsync();
            result
        };
        match result {
            Ok(()) => {
                self.refresh_session_blocks();
                self.set_notice(format!("workflow {name} started"), Role::Success);
            }
            Err(error) => self.set_notice(format!("workflow failed: {error}"), Role::Error),
        }
    }

    pub(super) fn command_sec(&mut self, rest: &str) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let paths = runtime.core.paths.clone();
        let security = runtime.core.security.clone();
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if parts.is_empty() {
            let fill = |label: &str, description: &str, text: &str| PickItem {
                label: label.into(),
                description: description.into(),
                action: PickAction::Fill(text.into()),
            };
            let picker = PickerModal::new(
                "sec",
                "Security",
                vec![
                    PickItem::run("sec", "summary", "show the engagement summary"),
                    PickItem::run("sec", "report", "write the engagement report"),
                    fill(
                        "scope add",
                        "add a target to scope (type it next)",
                        "/sec scope add ",
                    ),
                    fill(
                        "scope remove",
                        "remove a target from scope (type it next)",
                        "/sec scope remove ",
                    ),
                    PickItem {
                        label: "scope clear".into(),
                        description: "clear the scope".into(),
                        action: PickAction::Run("/sec scope clear".into()),
                    },
                ],
                "",
            );
            self.open_picker(picker, "");
            return;
        }
        if parts == ["summary"] {
            let summary = match security.lock() {
                Ok(state) => state.summary(),
                Err(poison) => poison.into_inner().summary(),
            };
            self.transcript.push_note("security", summary, false);
            return;
        }
        match parts[0] {
            "scope" => {
                let Some(action) = parts.get(1).copied() else {
                    self.command_sec_usage();
                    return;
                };
                match action {
                    "add" => {
                        let target = parts[2..].join(" ");
                        if target.is_empty() {
                            self.set_notice("usage: /sec scope add <target>", Role::Warning);
                            return;
                        }
                        let result = match security.lock() {
                            Ok(mut state) => state.add_scope(&paths, &target),
                            Err(poison) => poison.into_inner().add_scope(&paths, &target),
                        };
                        match result {
                            Ok(()) => {
                                self.set_notice(format!("scope added: {target}"), Role::Success);
                            }
                            Err(error) => {
                                self.set_notice(format!("scope add failed: {error}"), Role::Error);
                            }
                        }
                    }
                    "remove" => {
                        let target = parts[2..].join(" ");
                        if target.is_empty() {
                            self.set_notice("usage: /sec scope remove <target>", Role::Warning);
                            return;
                        }
                        let result = match security.lock() {
                            Ok(mut state) => state.remove_scope(&paths, &target),
                            Err(poison) => poison.into_inner().remove_scope(&paths, &target),
                        };
                        match result {
                            Ok(true) => {
                                self.set_notice(format!("scope removed: {target}"), Role::Success);
                            }
                            Ok(false) => {
                                self.set_notice(
                                    format!("scope did not contain {target}"),
                                    Role::Warning,
                                );
                            }
                            Err(error) => {
                                self.set_notice(
                                    format!("scope remove failed: {error}"),
                                    Role::Error,
                                );
                            }
                        }
                    }
                    "clear" => {
                        let result = match security.lock() {
                            Ok(mut state) => state.clear_scope(&paths),
                            Err(poison) => poison.into_inner().clear_scope(&paths),
                        };
                        match result {
                            Ok(()) => {
                                self.set_notice("scope cleared", Role::Success);
                            }
                            Err(error) => {
                                self.set_notice(
                                    format!("scope clear failed: {error}"),
                                    Role::Error,
                                );
                            }
                        }
                    }
                    other => {
                        self.set_notice(
                            format!("unknown scope action {other:?}; use add, remove, or clear"),
                            Role::Error,
                        );
                    }
                }
            }
            "report" => {
                let rendered = {
                    let mut state = match security.lock() {
                        Ok(state) => state,
                        Err(poison) => poison.into_inner(),
                    };
                    let name = state.engagement_name.clone();
                    let redact = state.redact;
                    match state.ensure_engagement(&paths) {
                        Ok(engagement) => Some((
                            name,
                            crate::security::render_report_with(engagement, redact),
                        )),
                        Err(error) => {
                            self.set_notice(
                                format!("cannot open engagement: {error}"),
                                Role::Error,
                            );
                            None
                        }
                    }
                };
                if let Some((name, markdown)) = rendered {
                    let path = paths.engagement_dir(&name).join("report.md");
                    let result = (|| -> std::io::Result<()> {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
                        std::fs::write(&path, markdown)?;
                        let _ =
                            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                        Ok(())
                    })();
                    match result {
                        Ok(()) => {
                            self.set_notice(
                                format!("report written to {}", path.display()),
                                Role::Success,
                            );
                        }
                        Err(error) => {
                            self.set_notice(format!("cannot write report: {error}"), Role::Error);
                        }
                    }
                }
            }
            _ => self.command_sec_usage(),
        }
    }

    pub(super) fn command_sec_usage(&mut self) {
        self.transcript.push_note(
            "security",
            "usage:\n/sec  (pick from a list)\n/sec summary\n/sec scope add|remove <target>\n/sec scope clear\n/sec report",
            false,
        );
    }

    pub(super) async fn command_verify(&mut self, rest: &str) {
        let Some(old) = self.core().cloned() else {
            self.set_notice("no runtime", Role::Error);
            return;
        };
        if rest.trim() == "accept" {
            match crate::verify_setup::accept_detected(&old.paths) {
                Ok((detected, _)) if detected.steps.is_empty() => {
                    self.set_notice(
                        "no verify steps detected; add [[steps]] to a [verify] config section",
                        Role::Warning,
                    );
                }
                Ok((detected, path)) => {
                    let mut fresh = self.config.clone();
                    fresh.verify.steps =
                        detected.steps.iter().map(|step| step.to_config()).collect();
                    match app::rebuild_core(&old, fresh).await {
                        Ok(core) => {
                            if let Some(runtime) = &mut self.runtime {
                                runtime.core = core.clone();
                            }
                            self.config = core.config.clone();
                            self.header.verify_off = false;
                            self.set_notice(
                                format!(
                                    "verify: saved {} in {}; the verify gate is active from the next turn",
                                    crate::verify_setup::describe(&detected),
                                    path.display()
                                ),
                                Role::Success,
                            );
                        }
                        Err(error) => {
                            self.set_notice(
                                format!("saved the steps, but the runtime did not reload: {error}"),
                                Role::Error,
                            );
                        }
                    }
                }
                Err(error) => {
                    self.set_notice(
                        format!("cannot save the verify steps: {error}"),
                        Role::Error,
                    );
                }
            }
            return;
        }
        let text = if self.config.verify.steps.is_empty() {
            crate::verify_setup::unconfigured_notice(
                &crate::tools::project::detect_verify_steps(&old.paths.cwd),
                "Type /verify accept to save them for this project.",
            )
        } else {
            let steps: Vec<String> = self
                .config
                .verify
                .steps
                .iter()
                .map(|step| format!("  {}: {}", step.name, step.command.join(" ")))
                .collect();
            format!(
                "verify: {} step(s) configured\n{}",
                steps.len(),
                steps.join("\n")
            )
        };
        self.transcript.push_note("verify", text, false);
    }

    pub(super) async fn command_trust(&mut self, rest: &str) {
        let trust = self.config.trust || self.options.trust;
        if !trust && rest.trim() == "confirm" {
            match self.accept_trust().await {
                Ok(()) => {
                    self.set_notice(
                        "trusted for this session: project config is loaded",
                        Role::Success,
                    );
                }
                Err(error) => {
                    self.set_notice(format!("cannot trust project: {error}"), Role::Error);
                }
            }
            return;
        }
        let text = if trust {
            "trusted: project .harness config, profiles, skills, workflows, and tools are loaded"
                .to_string()
        } else {
            format!(
                "untrusted: project .harness config, profiles, skills, workflows, and tools are ignored.\nType /trust confirm to load them for this session, or restart with --trust (TOT_TRUST=1). The global config is always trusted.\nProject directory: {}",
                self.config_paths_project()
            )
        };
        self.transcript.push_note("trust", text, false);
        self.set_notice(
            if trust {
                "trusted"
            } else {
                "untrusted; /trust confirm loads project config"
            },
            if trust { Role::Success } else { Role::Warning },
        );
    }

    pub(super) fn config_paths_project(&self) -> String {
        self.runtime
            .as_ref()
            .map(|runtime| runtime.core.paths.project_dir().display().to_string())
            .unwrap_or_else(|| ".harness".into())
    }

    pub(super) async fn accept_trust(&mut self) -> Result<()> {
        let Some(old) = self.core().cloned() else {
            bail!("no runtime");
        };
        let paths = old.paths.clone();
        let global = paths.config_file();
        let project = paths.project_dir().join("config.toml");
        let mut fresh = Config::load(&global, Some(&project))?;
        fresh.load_profile_dirs(
            &paths.global_profiles_dir(),
            Some(&paths.project_dir().join("profiles")),
        )?;
        let verify_path = paths.project_dir().join("verify.toml");
        if verify_path.exists() {
            fresh.verify = crate::config::VerifyConfig::load(&verify_path)?;
        }
        // Keep the runtime selections the user already made on the command line
        // or in this session.
        fresh.model.endpoint = self.config.model.endpoint.clone();
        fresh.model.model = self.config.model.model.clone();
        fresh.model.api_key = self.config.model.api_key.clone();
        fresh.model.context_window = self.config.model.context_window;
        fresh.profile_name = self.config.profile_name.clone();
        fresh.model_profile_override = self.config.model_profile_override.clone();
        fresh.ui.theme = self.config.ui.theme.clone();
        fresh.session = self.config.session.clone();
        fresh.trust = true;
        let core = app::rebuild_core(&old, fresh.clone()).await?;
        if let Some(runtime) = &mut self.runtime {
            runtime.core = core.clone();
        }
        self.config = core.config.clone();
        self.options.trust = true;
        self.options.skills =
            SkillIndex::load(&paths, true).advertised_for(&crate::tools::active_packs(&core.tools));
        self.header.trust = true;
        // A trusted project may bring its own .harness/verify.toml.
        self.header.verify_off = self.config.verify.steps.is_empty();
        self.header.profile = self.config.profile_name().to_string();
        self.header.tool_mode = core.tool_mode_label().unwrap_or_default();
        self.window = core.effective_window().0;
        self.header.window = self.window;
        self.used_tokens = estimate_used(&core);
        self.header.used = self.used_tokens;
        Ok(())
    }

    pub(super) fn command_export(&mut self, argument: &str) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let (markdown, id) = match runtime.session.lock() {
            Ok(session) => (session.export_markdown(), session.id.clone()),
            Err(poison) => {
                let session = poison.into_inner();
                (session.export_markdown(), session.id.clone())
            }
        };
        let cwd = runtime.core.paths.cwd.clone();
        let path = if argument.is_empty() {
            cwd.join(format!("totality-{id}.md"))
        } else {
            let requested = PathBuf::from(argument);
            if requested.is_absolute() {
                requested
            } else {
                cwd.join(requested)
            }
        };
        match std::fs::write(&path, markdown) {
            Ok(()) => {
                self.transcript
                    .push_note("export", format!("wrote {}", path.display()), false);
                self.set_notice(format!("exported {}", path.display()), Role::Success);
            }
            Err(error) => {
                self.set_notice(
                    format!("cannot write {}: {error}", path.display()),
                    Role::Error,
                );
            }
        }
    }

    pub(super) fn command_clear(&mut self) {
        if let Some(runtime) = &self.runtime {
            let floor = match runtime.session.lock() {
                Ok(session) => session
                    .tree
                    .entries()
                    .last()
                    .map(|entry| entry.seq)
                    .unwrap_or(0),
                Err(poison) => poison
                    .into_inner()
                    .tree
                    .entries()
                    .last()
                    .map(|entry| entry.seq)
                    .unwrap_or(0),
            };
            self.floor_seq = floor;
        }
        self.transcript.clear_view();
        self.set_notice("transcript cleared", Role::Muted);
    }

    pub(super) fn checkout_target(&mut self, target: &str, reason: &str) -> Result<(), String> {
        let result = {
            let Some(runtime) = &self.runtime else {
                return Err("no runtime".into());
            };
            let mut session = match runtime.session.lock() {
                Ok(session) => session,
                Err(poison) => poison.into_inner(),
            };
            let resolved = session
                .tree
                .resolve_checkout(target)
                .map_err(|error| error.to_string())?;
            session
                .checkout(&resolved, reason)
                .map_err(|error| error.to_string())?;
            let _ = session.fsync();
            Ok(())
        };
        if result.is_ok() {
            self.refresh_session_blocks();
        }
        result
    }

    pub(super) async fn switch_session(&mut self, path: PathBuf) {
        self.open_session(Some(path)).await;
    }

    /// Replace the runtime with one on `target`, or on a fresh session when
    /// `target` is `None`. On failure the previous session is reopened.
    pub(super) async fn open_session(&mut self, target: Option<PathBuf>) {
        if self.turn_active {
            self.set_notice(
                "cancel the running turn before switching sessions",
                Role::Warning,
            );
            return;
        }
        let Some(runtime) = &self.runtime else {
            return;
        };
        let config = self.config.clone();
        let paths = runtime.core.paths.clone();
        let path_jail = runtime.core.path_jail;
        let engagement = match runtime.core.security.lock() {
            Ok(security) => Some(security.engagement_name.clone()),
            Err(poison) => Some(poison.into_inner().engagement_name.clone()),
        };
        let old_resume = match runtime.session.lock() {
            Ok(session) => session.path.clone(),
            Err(poison) => poison.into_inner().path.clone(),
        };
        // An ephemeral (--no-session) run can start a new ephemeral session,
        // but cannot resume one from disk.
        let ephemeral = old_resume.is_none();
        if ephemeral && target.is_some() {
            self.set_notice("sessions are disabled (--no-session)", Role::Warning);
            return;
        }
        if let Some(runtime) = &self.runtime
            && let Ok(mut session) = runtime.session.lock()
        {
            let _ = session.fsync();
        }
        let old = self.runtime.take();
        drop(old);

        let options = RuntimeOptions {
            path_jail,
            engagement: engagement.clone(),
            no_session: ephemeral,
            no_redact: !config.session.redact,
            resume: target.clone(),
            continue_last: false,
            approver: None,
        };
        match app::create_runtime(config.clone(), paths.clone(), options).await {
            Ok(runtime) => {
                self.runtime = Some(runtime);
                self.after_session_switch();
                let notice = match &target {
                    Some(path) => format!("session: {}", path.display()),
                    None => format!("new session {}", self.header.session),
                };
                self.set_notice(notice, Role::Success);
            }
            Err(error) => {
                self.set_notice(
                    format!("cannot switch session: {error}; reopening the previous session"),
                    Role::Error,
                );
                let options = RuntimeOptions {
                    path_jail,
                    engagement,
                    no_session: ephemeral,
                    no_redact: !config.session.redact,
                    resume: old_resume,
                    continue_last: false,
                    approver: None,
                };
                match app::create_runtime(config, paths, options).await {
                    Ok(runtime) => {
                        self.runtime = Some(runtime);
                        self.after_session_switch();
                    }
                    Err(recover_error) => {
                        self.runtime = None;
                        self.quit = true;
                        self.fatal = Some(format!(
                            "cannot switch session ({error}) and cannot reopen the previous session ({recover_error})"
                        ));
                    }
                }
            }
        }
    }

    pub(super) fn after_session_switch(&mut self) {
        self.config = self
            .runtime
            .as_ref()
            .map(|runtime| runtime.core.config.clone())
            .unwrap_or_else(|| self.config.clone());
        self.floor_seq = 0;
        self.transcript = Transcript::new(self.config.ui.show_reasoning);
        self.update_from_core();
        self.refresh_session_blocks();
        let warnings = self
            .runtime
            .as_ref()
            .and_then(|runtime| {
                runtime
                    .session
                    .lock()
                    .ok()
                    .map(|session| session.warnings.clone())
            })
            .unwrap_or_default();
        for warning in warnings {
            self.transcript.push_note("session warning", warning, false);
        }
    }
}
