use super::*;
use ratatui::backend::TestBackend;
use ratatui_textarea::TextArea;

#[test]
fn bundled_workflows_are_discoverable() {
    let dir = tempfile::tempdir().unwrap();
    let mut paths = crate::paths::Paths::discover(dir.path()).unwrap();
    paths.config_dir = dir.path().join("config");
    let names = available_workflows(&paths, false);
    for expected in [
        "api-security",
        "coding",
        "general-assistance",
        "native-code-review",
        "security-blue",
        "security-red",
    ] {
        assert!(
            names.iter().any(|name| name == expected),
            "missing {expected}: {names:?}"
        );
    }
}

fn header(model: &str, session: &str) -> HeaderData {
    HeaderData {
        cwd: "/tmp/project".into(),
        git: GitStatus::none(),
        profile: "full".into(),
        model: model.into(),
        tool_mode: String::new(),
        logged_in: true,
        used: 12400,
        window: 64000,
        completion: 0,
        reasoning: 0,
        tok_s: 24.0,
        cache: Some((940, 1000)),
        session: session.into(),
        trust: true,
        window_guess: false,
    }
}

fn draw_scene(width: u16, height: u16, header: HeaderData, theme: Theme) -> Terminal<TestBackend> {
    let mut transcript = Transcript::new(false);
    transcript.set_viewport(height as usize);
    transcript.prepare(width as usize, &theme, false);
    let input = TextArea::new(vec![String::new()]);
    let footer = FooterData {
        hints: "Enter send".into(),
        status: String::new(),
        status_role: Role::Muted,
        notice: None,
        notice_role: Role::Warning,
    };
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            let scene = render::Scene {
                header: header.clone(),
                transcript: &transcript,
                input: &input,
                debug_lines: Vec::new(),
                footer: footer.clone(),
                completions: Vec::new(),
                completion_index: 0,
                completion_title: "",
                modal: None,
                activity: None,
                plan: Vec::new(),
                busy: false,
            };
            render::draw(frame, &scene, &theme);
        })
        .unwrap();
    terminal
}

fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    let area = *buffer.area();
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            if let Some(cell) = buffer.cell((x, y)) {
                out.push_str(cell.symbol());
            }
        }
        out.push('\n');
    }
    out
}

#[test]
fn header_at_120_contains_cwd_model_and_context() {
    let terminal = draw_scene(
        120,
        40,
        header("qwen3.5-35b-a3b", "abcdef12"),
        Theme::bundled("default"),
    );
    let text = buffer_text(&terminal);
    assert!(text.contains("/tmp/project"), "cwd missing:\n{text}");
    assert!(text.contains("qwen3.5-35b-a3b"), "model missing:\n{text}");
    assert!(text.contains("12.4k/64k"), "context missing:\n{text}");
    assert!(text.contains("tok/s"));
    assert!(
        text.contains("cache 940/1000"),
        "cached and total prompt tokens missing:\n{text}"
    );
    assert!(text.contains("▰"), "context gauge missing:\n{text}");
}

#[test]
fn narrow_header_drops_throughput_session_and_profile_but_keeps_required() {
    let terminal = draw_scene(
        60,
        20,
        header("qwen3.5-35b-a3b", "abcdef123456"),
        Theme::bundled("default"),
    );
    let text = buffer_text(&terminal);
    assert!(text.contains("/tmp/project"), "cwd missing:\n{text}");
    assert!(text.contains("qwen3.5-35b-a3b"), "model missing:\n{text}");
    assert!(text.contains("12.4k/64k"), "context missing:\n{text}");
    assert!(
        !text.contains("tok/s"),
        "throughput should be dropped:\n{text}"
    );
    assert!(
        !text.contains("abcdef123456"),
        "session id should be dropped:\n{text}"
    );
    assert!(!text.contains("full"), "profile should be dropped:\n{text}");
}

#[test]
fn theme_switch_changes_header_cell_style() {
    let before = draw_scene(
        120,
        20,
        header("model", "session"),
        Theme::bundled("default"),
    );
    let after = draw_scene(120, 20, header("model", "session"), Theme::bundled("light"));
    // The info bar is the second-to-last row; column 1 starts the cwd.
    let before_cell = before.backend().buffer().cell((1, 18)).unwrap();
    let after_cell = after.backend().buffer().cell((1, 18)).unwrap();
    assert_eq!(before_cell.symbol(), "/");
    assert_eq!(after_cell.symbol(), "/");
    assert_ne!(
        before_cell.style().fg,
        after_cell.style().fg,
        "theme switch should recolor the header"
    );
}

#[test]
fn header_shows_not_logged_in_without_a_model() {
    let mut header = header("", "session");
    header.logged_in = false;
    let terminal = draw_scene(120, 20, header, Theme::bundled("default"));
    let text = buffer_text(&terminal);
    assert!(text.contains("not logged in"), "header:\n{text}");
}

async fn model_list(
    axum::extract::State(models): axum::extract::State<Arc<Vec<String>>>,
) -> axum::response::Response {
    let data: Vec<serde_json::Value> = models
        .iter()
        .map(|id| serde_json::json!({"id": id, "object": "model"}))
        .collect();
    let body = serde_json::json!({"object": "list", "data": data}).to_string();
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))
        .unwrap()
}

async fn model_endpoint(models: &[&str]) -> String {
    let state: Arc<Vec<String>> = Arc::new(models.iter().map(|m| m.to_string()).collect());
    let app = axum::Router::new()
        .route("/v1/models", axum::routing::get(model_list))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}")
}

/// A `/v1/models` endpoint whose entries advertise `max_model_len`, like vLLM.
async fn sized_model_endpoint(models: &[(&str, u64)]) -> String {
    let data: Vec<serde_json::Value> = models
        .iter()
        .map(|(id, ctx)| serde_json::json!({"id": id, "object": "model", "max_model_len": ctx}))
        .collect();
    let body = Arc::new(serde_json::json!({"object": "list", "data": data}).to_string());
    let app = axum::Router::new().route(
        "/v1/models",
        axum::routing::get(move || {
            let body = body.clone();
            async move {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    body.as_ref().clone(),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}")
}

#[tokio::test]
async fn switching_models_reprobes_the_context_window() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let paths = crate::paths::Paths {
        config_dir: dir.path().join("config"),
        data_dir: dir.path().join("data"),
        cwd,
        project_id: "test".into(),
        sessions_dir_override: None,
    };
    let base = sized_model_endpoint(&[("small", 8192), ("big", 131072)]).await;
    let mut config = Config::builtin();
    config.model.endpoint = base;
    config.model.model = "small".into();
    let runtime = app::create_runtime(
        config,
        paths,
        RuntimeOptions {
            no_session: true,
            ..RuntimeOptions::default()
        },
    )
    .await
    .unwrap();
    let mut app = App::new(
        runtime,
        UiOptions {
            no_color: true,
            trust: false,
            skills: SkillIndex::bundled(),
        },
    )
    .unwrap();
    assert_eq!(app.header.window, 8192);
    // The session header records all four context values.
    let header = app
        .core()
        .unwrap()
        .session
        .lock()
        .unwrap()
        .header()
        .cloned()
        .expect("session header");
    let context = header.context.expect("context recorded");
    assert_eq!(context.allocated, Some(8192));
    assert_eq!(context.effective, 8192);
    // The probe result is not written back as if the user configured it.
    assert_eq!(app.config.model.context_window, None);

    app.switch_model("big").await;
    // The rebuild runs in the background; the TUI stays live meanwhile.
    assert!(app.switching.is_some());
    assert!(app.activity().is_some_and(|a| a.label.contains("big")));
    app.settle().await;
    assert!(app.switching.is_none());
    assert_eq!(app.header.model, "big");
    assert_eq!(app.header.window, 131072);
    assert_eq!(app.core().unwrap().budget.context_window, 131072);

    // An explicit value is clamped to what the server allocated.
    let mut pinned = app.config.clone();
    pinned.model.context_window = Some(20_000);
    pinned.model.model = "small".into();
    app.rebuild_core_with(pinned).await.unwrap();
    assert_eq!(app.header.window, 8192);
    assert_eq!(app.core().unwrap().budget.context_window, 8192);
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn picker_filters_ranks_and_clamps_selection() {
    let models: Vec<String> = vec![
        "openai/gpt-x".into(),
        "anthropic/claude".into(),
        "local/qwen3".into(),
    ];
    let mut view = PickerModal::new("model", "Models", model_items(&models), "local/qwen3")
        .with_subtitle("http://gw");
    assert_eq!(view.selected_label().as_deref(), Some("local/qwen3"));
    view.push_filter('A');
    view.push_filter('n');
    let labels: Vec<&str> = view.filtered().iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, vec!["anthropic/claude"]);
    view.move_by(5);
    assert_eq!(
        view.selected_item().map(|item| item.action.clone()),
        Some(PickAction::Run("/model anthropic/claude".into()))
    );
    view.push_filter('z');
    assert!(view.selected_item().is_none());
    view.pop_filter();
    view.pop_filter();
    view.pop_filter();
    assert_eq!(view.filtered().len(), 3);

    // Label prefixes rank above substrings and description matches.
    let mut view = PickerModal::new(
        "x",
        "X",
        vec![
            PickItem::run("x", "dark-blue", ""),
            PickItem::run("x", "plain", "a dark look"),
            PickItem::run("x", "darker", ""),
        ],
        "",
    );
    view.push_filter('d');
    view.push_filter('a');
    view.push_filter('r');
    let labels: Vec<&str> = view.filtered().iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, vec!["dark-blue", "darker", "plain"]);

    // A refresh keeps the highlighted row.
    view.move_by(1);
    view.replace_items(vec![
        PickItem::run("x", "darker", ""),
        PickItem::run("x", "dark-blue", ""),
    ]);
    assert_eq!(view.selected_label().as_deref(), Some("darker"));
}

#[test]
fn match_session_by_prefix_suffix_and_ambiguity() {
    let item = |id: &str| SessionItem {
        id: id.into(),
        preview: String::new(),
        when: String::new(),
        path: PathBuf::from(format!("{id}.jsonl")),
    };
    let items = vec![item("20261108-aaaa1111"), item("20261107-bbbb2222")];
    assert_eq!(
        match_session(&items, "aaaa1111").unwrap(),
        PathBuf::from("20261108-aaaa1111.jsonl")
    );
    assert_eq!(
        match_session(&items, "20261107").unwrap(),
        PathBuf::from("20261107-bbbb2222.jsonl")
    );
    assert!(
        match_session(&items, "2026")
            .unwrap_err()
            .contains("matches 2 sessions")
    );
    assert!(match_session(&items, "zzz").is_err());
}

#[tokio::test]
async fn tab_completes_commands_arguments_and_paths() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(cwd.join("src/ui")).unwrap();
    std::fs::write(cwd.join("src/ui/input.rs"), "").unwrap();
    std::fs::write(cwd.join("src/main.rs"), "").unwrap();
    let paths = crate::paths::Paths {
        config_dir: dir.path().join("config"),
        data_dir: dir.path().join("data"),
        cwd: cwd.clone(),
        project_id: "test".into(),
        sessions_dir_override: None,
    };
    let base = model_endpoint(&["m"]).await;
    let mut config = Config::builtin();
    config.model.endpoint = base;
    config.model.model = "m".into();
    let runtime = app::create_runtime(config, paths.clone(), RuntimeOptions::default())
        .await
        .unwrap();
    let options = || UiOptions {
        no_color: true,
        trust: false,
        skills: SkillIndex::bundled(),
    };
    let mut app = App::new(runtime, options()).unwrap();
    let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
    let back_tab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);

    // A unique command completes with a separating space.
    app.input.set_text("/them");
    app.handle_key(tab).await;
    assert_eq!(app.input.text(), "/theme ");

    // Arguments complete from the theme list.
    app.input.set_text("/theme gru");
    app.handle_key(tab).await;
    assert_eq!(app.input.text(), "/theme gruvbox-dark ");
    // No match inside a command: Tab inserts nothing.
    app.input.set_text("/theme zzz");
    app.handle_key(tab).await;
    assert_eq!(app.input.text(), "/theme zzz");

    // Ambiguous commands: Shift+Tab moves the selection back, Tab accepts.
    app.input.set_text("/tr");
    app.handle_key(back_tab).await;
    app.handle_key(tab).await;
    assert_eq!(app.input.text(), "/trust ");

    // Enter on a partial argument runs the highlighted completion.
    app.input.set_text("/theme high");
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.theme.name, "high-contrast");

    // Paths: the first Tab extends to the shared prefix, the popup lists
    // the rest, and a unique file is accepted.
    app.input.set_text("explain @sr");
    app.handle_key(tab).await;
    assert_eq!(app.input.text(), "explain @src/");
    assert!(app.current_completion().is_none());
    app.handle_key(tab).await;
    let completion = app.current_completion().expect("path popup");
    assert_eq!(completion.candidates[0].value, "src/ui/");
    app.input.set_text("explain @src/m");
    app.handle_key(tab).await;
    assert_eq!(app.input.text(), "explain @src/main.rs ");

    // Plain prose: Tab falls through to the textarea.
    app.input.set_text("hello");
    app.handle_key(tab).await;
    assert!(app.input.text().starts_with("hello"));

    // Prompt history survives a restart for the same project.
    app.input.set_text("remember me");
    app.input.submit(true).unwrap();
    drop(app);
    let runtime = app::create_runtime(
        {
            let mut config = Config::builtin();
            config.model.endpoint = model_endpoint(&["m"]).await;
            config.model.model = "m".into();
            config
        },
        paths,
        RuntimeOptions::default(),
    )
    .await
    .unwrap();
    let mut app = App::new(runtime, options()).unwrap();
    assert!(app.input.history_prev());
    assert_eq!(app.input.text(), "remember me");
}

async fn test_app() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let paths = crate::paths::Paths {
        config_dir: dir.path().join("config"),
        data_dir: dir.path().join("data"),
        cwd,
        project_id: "test".into(),
        sessions_dir_override: None,
    };
    let mut config = Config::builtin();
    config.model.endpoint = model_endpoint(&["m"]).await;
    config.model.model = "m".into();
    let runtime = app::create_runtime(
        config,
        paths,
        RuntimeOptions {
            no_session: true,
            ..RuntimeOptions::default()
        },
    )
    .await
    .unwrap();
    let app = App::new(
        runtime,
        UiOptions {
            no_color: true,
            trust: false,
            skills: SkillIndex::bundled(),
        },
    )
    .unwrap();
    (dir, app)
}

/// A terminal emulator fed exactly the bytes the TUI writes, compared
/// with the frame ratatui believes it drew. Any disagreement is a stale
/// or misplaced character on a real screen.
struct EmulatedScreen {
    terminal: Terminal<SizedBackend>,
    output: SharedOutput,
    parser: vt100::Parser,
}

/// The real crossterm backend writing into `SharedOutput`, with a size
/// set by the test instead of read from a tty. Querying the cursor fails
/// the test: real terminals may never answer, so the TUI must not ask.
struct SizedBackend {
    inner: CrosstermBackend<SharedOutput>,
    size: ratatui::layout::Size,
}

impl std::io::Write for SizedBackend {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        std::io::Write::write(&mut self.inner, bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(&mut self.inner)
    }
}

impl ratatui::backend::Backend for SizedBackend {
    type Error = std::io::Error;
    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.inner.draw(content)
    }
    fn hide_cursor(&mut self) -> std::io::Result<()> {
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> std::io::Result<()> {
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
        panic!("the TUI queried the cursor position; a silent terminal would stall here")
    }
    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        position: P,
    ) -> std::io::Result<()> {
        self.inner.set_cursor_position(position)
    }
    fn clear(&mut self) -> std::io::Result<()> {
        self.inner.clear()
    }
    fn clear_region(&mut self, clear_type: ratatui::backend::ClearType) -> std::io::Result<()> {
        self.inner.clear_region(clear_type)
    }
    fn size(&self) -> std::io::Result<ratatui::layout::Size> {
        Ok(self.size)
    }
    fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
        Ok(ratatui::backend::WindowSize {
            columns_rows: self.size,
            pixels: ratatui::layout::Size::default(),
        })
    }
    fn flush(&mut self) -> std::io::Result<()> {
        ratatui::backend::Backend::flush(&mut self.inner)
    }
}

/// Bytes written to the "terminal", readable by the test.
#[derive(Clone, Default)]
struct SharedOutput(std::rc::Rc<RefCell<Vec<u8>>>);

impl std::io::Write for SharedOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl EmulatedScreen {
    fn new(width: u16, height: u16) -> Self {
        let output = SharedOutput::default();
        let terminal = Terminal::new(SizedBackend {
            inner: CrosstermBackend::new(output.clone()),
            size: ratatui::layout::Size::new(width, height),
        })
        .unwrap();
        let parser = vt100::Parser::new(
            std::num::NonZeroU16::new(height).unwrap(),
            std::num::NonZeroU16::new(width).unwrap(),
            0,
        );
        EmulatedScreen {
            terminal,
            output,
            parser,
        }
    }

    /// The window changes size; the TUI notices on its next frame.
    fn resize(&mut self, width: u16, height: u16) {
        self.terminal.backend_mut().size = ratatui::layout::Size::new(width, height);
        self.parser.screen_mut().set_size(
            std::num::NonZeroU16::new(height).unwrap(),
            std::num::NonZeroU16::new(width).unwrap(),
        );
    }

    /// Draw one frame and check the emulated screen against it.
    fn frame(&mut self, app: &mut App, step: &str) {
        if let Err(error) = app.draw(&mut self.terminal) {
            panic!("{step}: draw failed: {error:#}");
        }
        let bytes = std::mem::take(&mut *self.output.0.borrow_mut());
        self.parser.process(&bytes);
        let frame = app.last_frame.as_ref().expect("a drawn frame");
        let area = frame.area;
        for y in 0..area.height {
            let mut expected = String::new();
            let mut emulated = String::new();
            let mut skip = 0;
            for x in 0..area.width {
                let cell = &frame[(x, y)];
                let symbol = cell.symbol();
                // Everything painted must be free of text the terminal
                // could measure differently.
                assert!(
                    !crate::sanitize::needs_cleaning(symbol),
                    "{step}: unsafe text {symbol:?} painted at ({x}, {y})"
                );
                if skip > 0 {
                    skip -= 1;
                } else {
                    expected.push_str(symbol);
                    skip = unicode_width::UnicodeWidthStr::width(symbol).saturating_sub(1);
                }
                let cell = self.parser.screen().cell(y, x).expect("cell");
                if !cell.is_wide_continuation() {
                    let contents = cell.contents();
                    emulated.push_str(if contents.is_empty() { " " } else { contents });
                }
            }
            assert_eq!(
                emulated.trim_end(),
                expected.trim_end(),
                "{step}: row {y} differs from what ratatui drew"
            );
        }
    }
}

/// Text models and tools really produce, chosen to break width
/// assumptions: escapes, tabs, carriage returns, emoji sequences, CJK,
/// combining marks, bidi controls, and Markdown that reflows.
const HOSTILE: &[&str] = &[
    "plain words and more words ",
    "\u{26a0}\u{fe0f} warning ",
    "\u{1f468}\u{200d}\u{1f4bb} dev ",
    "\u{1f44d}\u{1f3fd} ",
    "\u{1f1e7}\u{1f1f7} flag ",
    "\t tabbed\t",
    "\u{1b}[1;31mred\u{1b}[0m ",
    "\u{1b}]8;;https://example.com\u{7}link\u{1b}]8;;\u{7} ",
    "progress 10%\rprogress 90%",
    "日本語のテキスト ",
    "e\u{301}cole ",
    "\u{200b}zero\u{feff}width ",
    "\u{202e}reversed\u{202c} ",
    "\n",
    "\n\n",
    "\n\n## Heading ✅\n\n",
    "\n\n```rust\nfn main() {\n\tlet x = \"日本\";\n}\n```\n\n",
    "\n- item one\n- item two\n",
    "\n| a | b |\n|---|---|\n| 日本 | \u{26a0}\u{fe0f} |\n",
    "averyveryveryveryveryveryveryveryveryveryveryveryveryveryveryverylongword ",
    "\r\n",
];

#[tokio::test]
async fn screen_never_drifts_from_what_ratatui_drew() {
    let (_dir, mut app) = test_app().await;
    let mut screen = EmulatedScreen::new(72, 24);
    screen.frame(&mut app, "first frame");
    // A tiny deterministic generator: reproducible without a dependency.
    let mut seed: u64 = 0x5eed_1234_abcd_0001;
    let mut next = move |bound: usize| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((seed >> 33) as usize) % bound
    };
    app.transcript
        .begin_turn("check the hostile text \u{26a0}\u{fe0f}\t\u{1b}[1mnow\u{1b}[0m");
    let mut tools = 0;
    for step in 0..600 {
        let piece = HOSTILE[next(HOSTILE.len())];
        let label = format!("step {step} after {piece:?}");
        match next(20) {
            0 => {
                tools += 1;
                let id = format!("t{tools}");
                app.transcript.tool_started(&id, "bash", piece);
                screen.frame(&mut app, &label);
                let preview: String = (0..4).map(|_| HOSTILE[next(HOSTILE.len())]).collect();
                app.transcript
                    .tool_finished(&id, "bash", next(2) == 0, 1200, &preview, None);
            }
            1 => app.set_notice(piece, Role::Warning),
            2 => app.handle_event(Event::Paste(piece.to_string())).await,
            3 => {
                app.handle_event(Event::Key(KeyEvent::new(
                    KeyCode::Char('l'),
                    KeyModifiers::CONTROL,
                )))
                .await
            }
            4 => {
                let (width, height) = [(72, 24), (40, 12), (100, 30), (61, 17)][next(4)];
                screen.resize(width, height);
                app.handle_event(Event::Resize(width, height)).await;
            }
            5 => app.transcript.scroll_up(1 + next(10)),
            6 => app.transcript.scroll_down(1 + next(10)),
            7 if next(4) == 0 => {
                app.transcript.finish_turn();
                app.transcript.begin_turn(piece);
            }
            _ => app.transcript.text_delta(piece),
        }
        screen.frame(&mut app, &label);
    }
}

#[tokio::test]
async fn list_commands_open_pickers_and_pick_without_typing() {
    let (_dir, mut app) = test_app().await;
    let enter = key(KeyCode::Enter);

    // /theme previews as the highlight moves; Esc restores.
    let original = app.theme.name.clone();
    app.input.set_text("/theme");
    app.handle_key(enter).await;
    assert!(matches!(&app.modal, Some(ModalData::Picker(v)) if v.command == "theme"));
    app.handle_modal_key(key(KeyCode::Down)).await;
    assert_ne!(app.theme.name, original, "moving previews the theme");
    app.handle_modal_key(key(KeyCode::Esc)).await;
    assert!(app.modal.is_none());
    assert_eq!(app.theme.name, original, "Esc restores the theme");

    // Filtering and Enter applies the pick.
    app.command_theme("");
    for ch in "gruvbox-d".chars() {
        app.handle_modal_key(key(KeyCode::Char(ch))).await;
    }
    app.handle_modal_key(enter).await;
    assert!(app.modal.is_none());
    assert_eq!(app.theme.name, "gruvbox-dark");

    // /profile lists profiles with the current one preselected.
    app.command_profile("").await;
    match &app.modal {
        Some(ModalData::Picker(view)) => {
            assert!(view.items.len() > 1);
            assert_eq!(
                view.selected_label().as_deref(),
                Some(app.config.profile_name())
            );
        }
        _ => panic!("expected the profile picker"),
    }
    app.modal = None;

    // /sec: summary runs directly; scope add fills the prompt.
    app.command_sec("");
    for ch in "scope add".chars() {
        app.handle_modal_key(key(KeyCode::Char(ch))).await;
    }
    app.handle_modal_key(enter).await;
    assert!(app.modal.is_none());
    assert_eq!(app.input.text(), "/sec scope add ");
    app.input.set_text("");

    // /skill lists the skills the profile's packs advertise.
    let mut full = app.config.clone();
    full.profile_name = "full".into();
    app.rebuild_core_with(full).await.unwrap();
    app.command_skill("");
    match &app.modal {
        Some(ModalData::Picker(view)) => {
            assert!(!view.items.is_empty());
            assert!(view.items.iter().all(|item| item.action
                == PickAction::Run(format!("/skill {}", item.label))
                && !item.description.is_empty()));
        }
        _ => panic!("expected the skill picker"),
    }
    app.modal = None;

    // /help <command> explains one command.
    app.command_help("theme");
    app.transcript.prepare(120, &app.theme, false);
    let notes = format!("{:?}", app.transcript.lines());
    assert!(notes.contains("live preview"), "{notes}");

    // Esc abandons a half-typed command.
    app.input.set_text("/the");
    app.handle_key(key(KeyCode::Esc)).await;
    assert!(app.input.is_empty());

    // Idle ticks do not ask for frames; a running switch animates.
    app.notice = None;
    app.transcript.prepare(80, &app.theme, false);
    assert!(!app.on_tick());
}

#[tokio::test]
async fn new_and_resume_switch_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let paths = crate::paths::Paths {
        config_dir: dir.path().join("config"),
        data_dir: dir.path().join("data"),
        cwd,
        project_id: "test".into(),
        sessions_dir_override: None,
    };
    let base = model_endpoint(&["m"]).await;
    let mut config = Config::builtin();
    config.model.endpoint = base;
    config.model.model = "m".into();
    let runtime = app::create_runtime(config, paths, RuntimeOptions::default())
        .await
        .unwrap();
    let mut app = App::new(
        runtime,
        UiOptions {
            no_color: true,
            trust: false,
            skills: SkillIndex::bundled(),
        },
    )
    .unwrap();
    let session_path = |app: &App| {
        app.runtime
            .as_ref()
            .unwrap()
            .session
            .lock()
            .unwrap()
            .path
            .clone()
            .unwrap()
    };
    let first = session_path(&app);

    // /new on an untouched session does not litter another file.
    app.command_new().await;
    assert_eq!(session_path(&app), first);

    app.runtime
        .as_ref()
        .unwrap()
        .session
        .lock()
        .unwrap()
        .append(EntryKind::User(UserData {
            attachments: Vec::new(),
            content: "first session message".into(),
        }))
        .unwrap();
    app.command_new().await;
    let second = session_path(&app);
    assert_ne!(second, first);

    // /resume lists only the other sessions; /resume last reopens the first.
    app.command_resume("").await;
    match &app.modal {
        Some(ModalData::Sessions(view)) => {
            assert_eq!(view.items.len(), 1);
            assert_eq!(view.items[0].path, first);
            assert_eq!(view.items[0].preview, "first session message");
        }
        _ => panic!("expected the sessions picker"),
    }
    app.modal = None;
    app.command_resume("last").await;
    assert_eq!(session_path(&app), first);
}

#[tokio::test]
async fn login_configures_a_model_and_model_command_lists_choices() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let config_dir = dir.path().join("config");
    let config_path = config_dir.join("config.toml");
    let paths = crate::paths::Paths {
        config_dir,
        data_dir: dir.path().join("data"),
        cwd,
        project_id: "test".into(),
        sessions_dir_override: None,
    };

    // Reserve a port and close it so the initial endpoint is unreachable.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let down = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);

    let mut config = Config::builtin();
    config.model.endpoint = down;
    config.model.model.clear();
    let runtime = app::create_runtime(
        config,
        paths,
        RuntimeOptions {
            no_session: true,
            ..RuntimeOptions::default()
        },
    )
    .await
    .unwrap();
    let mut app = App::new(
        runtime,
        UiOptions {
            no_color: true,
            trust: false,
            skills: SkillIndex::bundled(),
        },
    )
    .unwrap();
    assert!(!app.header.logged_in);

    app.start_turn("hello".into());
    assert!(!app.turn_active, "turns must be blocked before login");

    // /login validates the endpoint, selects the named model, and saves.
    let base = model_endpoint(&["alpha", "beta"]).await;
    app.command_login(&format!("{base} beta")).await;
    assert!(app.header.logged_in);
    assert_eq!(app.header.model, "beta");
    assert_eq!(app.config.model.endpoint, base);

    // /model with no argument opens a picker over the endpoint's models,
    // with the current model preselected.
    app.command_model("").await;
    match &app.modal {
        Some(ModalData::Picker(view)) => {
            let labels: Vec<&str> = view.items.iter().map(|i| i.label.as_str()).collect();
            assert_eq!(labels, vec!["alpha", "beta"]);
            assert_eq!(view.selected_label().as_deref(), Some("beta"));
        }
        _ => panic!("expected the models picker"),
    }
    // Typing filters; Enter switches the session's model.
    for ch in "alp".chars() {
        app.handle_modal_key(key(KeyCode::Char(ch))).await;
    }
    app.handle_modal_key(key(KeyCode::Enter)).await;
    assert!(app.modal.is_none());
    app.settle().await;
    assert_eq!(app.header.model, "alpha");
    // The picker switch lasts for the session; the saved config keeps beta.
    let saved = std::fs::read_to_string(&config_path).unwrap();
    assert!(saved.contains("beta"), "config:\n{saved}");

    // A login without a model auto-selects the endpoint's only model and
    // replaces the previous endpoint's selection.
    let single = model_endpoint(&["only-model"]).await;
    app.command_login(&single).await;
    assert_eq!(app.header.model, "only-model");
    let saved = std::fs::read_to_string(&config_path).unwrap();
    let parsed: toml::Value = toml::from_str(&saved).unwrap();
    assert_eq!(parsed["model"]["endpoint"].as_str(), Some(single.as_str()));
    assert_eq!(parsed["model"]["model"].as_str(), Some("only-model"));
}

fn tool_names(app: &App) -> Vec<&'static str> {
    app.core()
        .unwrap()
        .tools
        .iter()
        .map(|tool| tool.name())
        .collect()
}

#[tokio::test]
async fn plan_approve_restores_exactly_the_prior_tool_set() {
    let (_dir, mut app) = test_app().await;
    let before = tool_names(&app);
    assert!(before.contains(&"write"));

    app.run_command("/plan").await;
    let planning = tool_names(&app);
    for removed in ["write", "edit", "patch", "undo", "bg"] {
        assert!(!planning.contains(&removed), "{removed} in {planning:?}");
    }
    assert!(planning.contains(&"read") && planning.contains(&"todo"));
    assert!(
        app.footer().status.contains("/plan approve"),
        "{}",
        app.footer().status
    );

    // A workflow that lists write cannot bring it back while planning.
    let mut config = app.config.clone();
    let workflow = WorkflowSpec {
        name: "writer".into(),
        description: None,
        profile: None,
        prompt: "write".into(),
        skills: vec![],
        tools: vec!["write".into(), "read".into()],
        steps: vec![],
        artifacts: vec![],
    };
    workflow.apply_to_config(&mut config).unwrap();
    app.rebuild_core_with(config).await.unwrap();
    assert!(!tool_names(&app).contains(&"write"));
    app.rebuild_core_with({
        let mut config = app.config.clone();
        config.profile_name = "coding".into();
        config
    })
    .await
    .unwrap();

    app.run_command("/plan approve").await;
    assert_eq!(tool_names(&app), before);
    assert_eq!(
        app.core().unwrap().permissions.mode(),
        crate::policy::PermissionMode::Ask
    );
    let approved = app
        .runtime
        .as_ref()
        .unwrap()
        .session
        .lock()
        .ok()
        .and_then(|session| crate::tools::todo::latest_plan(&session));
    assert_eq!(
        approved.map(|(status, _)| status).as_deref(),
        Some("approved")
    );

    // /plan exit also restores the tools and discards the plan.
    app.run_command("/plan").await;
    app.run_command("/plan exit").await;
    assert_eq!(tool_names(&app), before);
}

#[tokio::test]
async fn a_permission_prompt_shows_the_exact_command_and_answers_with_y_or_n() {
    let (_dir, mut app) = test_app().await;
    let ask = |subject: &str| {
        let (reply, answer) = tokio::sync::oneshot::channel();
        let pending = permission::PendingAsk {
            request: crate::policy::AskRequest {
                tool: "bash".into(),
                subject: subject.into(),
                preview: None,
                rule: Some("push-asks".into()),
                key: format!("bash\0{subject}"),
            },
            reply,
        };
        (pending, answer)
    };

    let (pending, answer) = ask("git push origin feature/x");
    app.show_permission_request(pending);
    let mut screen = EmulatedScreen::new(100, 30);
    screen.frame(&mut app, "ask shown");
    let shown = screen.parser.screen().contents();
    assert!(shown.contains("$ git push origin feature/x"), "{shown}");
    assert!(shown.contains("push-asks"), "{shown}");
    assert!(shown.contains("y allow"), "{shown}");
    // A stray key does not answer; y does.
    app.handle_key(key(KeyCode::Char('q'))).await;
    assert!(app.pending_ask.is_some());
    app.handle_key(key(KeyCode::Char('y'))).await;
    assert_eq!(answer.await.unwrap(), crate::policy::AskAnswer::Allow);

    let (pending, answer) = ask("rm -rf build");
    app.show_permission_request(pending);
    app.handle_key(key(KeyCode::Char('n'))).await;
    assert_eq!(answer.await.unwrap(), crate::policy::AskAnswer::Deny);
    assert!(app.pending_ask.is_none());
}

#[test]
fn ask_text_includes_the_diff_preview() {
    let request = crate::policy::AskRequest {
        tool: "edit".into(),
        subject: "src/lib.rs".into(),
        preview: crate::policy::preview(
            "edit",
            &serde_json::json!({"path": "src/lib.rs", "old": "a\n", "new": "b\n"}),
            std::path::Path::new("/w"),
            40,
        ),
        rule: None,
        key: String::new(),
    };
    let text = permission::ask_text(&request);
    assert!(text.starts_with("edit: src/lib.rs"), "{text}");
    assert!(text.contains("-a") && text.contains("+b"), "{text}");
}
