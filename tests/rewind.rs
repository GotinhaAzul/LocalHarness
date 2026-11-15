//! `/rewind` integration tests: conversation leaf, files, conflicts, backups.

use std::sync::{Arc, Mutex};

use localharness::config::VerifyConfig;
use localharness::paths::Paths;
use localharness::security::SecurityState;
use localharness::session::rewind;
use localharness::session::{AssistantData, EntryKind, Session, SessionData, UserData};
use localharness::tools::edit::EditTool;
use localharness::tools::read::ReadTool;
use localharness::tools::undo::fs_change_entry;
use localharness::tools::write::WriteTool;
use localharness::tools::{BgRegistry, ReadTracker, SessionHandle, Tool, ToolContext};
use serde_json::json;
use tokio_util::sync::CancellationToken;

struct Fixture {
    _dir: tempfile::TempDir,
    dir: std::path::PathBuf,
    session: SessionHandle,
    ctx: ToolContext,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let paths = Paths {
            config_dir: cwd.join(".lh-config"),
            data_dir: cwd.join(".lh-data"),
            cwd: cwd.clone(),
            project_id: "test".into(),
            sessions_dir_override: None,
            home_dir: None,
        };
        let header = SessionData {
            cwd: cwd.display().to_string(),
            endpoint: "test".into(),
            model: "test".into(),
            profile: "test".into(),
            app: "test".into(),
            redact: true,
            context: None,
            permissions: None,
        };
        let session = Session::create(&paths, header, true).expect("in-memory session");
        let artifacts = session.artifacts.clone();
        let session: SessionHandle = Arc::new(Mutex::new(session));
        let ctx = ToolContext {
            cwd: cwd.clone(),
            path_jail: true,
            session: session.clone(),
            artifacts,
            cancel: CancellationToken::new(),
            trust: false,
            verify: VerifyConfig { steps: Vec::new() },
            profile: "test".into(),
            packs: Vec::new(),
            reads: Arc::new(Mutex::new(ReadTracker::default())),
            bg: Arc::new(Mutex::new(BgRegistry::default())),
            security: Arc::new(Mutex::new(SecurityState::default())),
            paths,
            lsp: Arc::new(localharness::tools::lsp::LspService::new()),
            sandbox: None,
            vision: Default::default(),
            edit_mode: localharness::config::EditMode::Text,
        };
        Fixture {
            _dir: dir,
            dir: cwd,
            session,
            ctx,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Session> {
        self.session.lock().unwrap()
    }

    fn user(&self, text: &str) -> String {
        self.lock()
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
                content: text.into(),
            }))
            .unwrap()
            .id
    }

    fn assistant(&self) -> String {
        self.lock()
            .append(EntryKind::Assistant(AssistantData {
                content: Some("ok".into()),
                ..AssistantData::default()
            }))
            .unwrap()
            .id
    }

    fn continuation(&self, text: &str) -> String {
        self.lock()
            .append_tagged(
                EntryKind::User(UserData {
                    attachments: Vec::new(),
                    content: text.into(),
                }),
                Some(localharness::provenance::CONTINUATION.to_string()),
            )
            .unwrap()
            .id
    }

    /// Run a mutating tool and record its `fs-change` entry the way the agent
    /// loop does, so undo and rewind see the same session shape.
    async fn mutate(&self, tool: &dyn Tool, args: serde_json::Value) {
        let result = tool.call(&self.ctx, args.clone()).await;
        assert!(!result.is_error, "{}", result.content);
        let display = args["path"].as_str().unwrap().to_string();
        let entry = fs_change_entry(display, result.details.as_ref());
        self.lock().append(EntryKind::Custom(entry)).unwrap();
    }

    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.join(path)).ok()
    }

    fn write(&self, path: &str, content: &str) {
        std::fs::write(self.dir.join(path), content).unwrap();
    }
}

#[tokio::test]
async fn rewinding_two_turns_restores_conversation_and_files() {
    let fixture = Fixture::new();
    fixture.write("a.rs", "let a = 1;\n");
    fixture.write("outside.txt", "mine\n");
    let read = ReadTool.call(&fixture.ctx, json!({"path": "a.rs"})).await;
    assert!(!read.is_error, "{}", read.content);

    // Turn one: edit a.rs.
    let first_user = fixture.user("edit a");
    fixture.assistant();
    fixture
        .mutate(&EditTool, json!({"path": "a.rs", "old": "1", "new": "2"}))
        .await;
    let first_assistant = fixture
        .lock()
        .branch_entries()
        .into_iter()
        .rfind(|entry| entry.is_assistant())
        .map(|entry| entry.id.clone())
        .expect("first turn assistant");
    let first_leaf = fixture.lock().active_leaf_id().unwrap();

    // Turn two: create b.rs and edit a.rs again.
    fixture.user("edit b");
    fixture.assistant();
    fixture
        .mutate(
            &WriteTool,
            json!({"path": "b.rs", "content": "fn b() {}\n"}),
        )
        .await;
    fixture
        .mutate(&EditTool, json!({"path": "a.rs", "old": "2", "new": "3"}))
        .await;
    assert_eq!(fixture.read("a.rs").unwrap(), "let a = 3;\n");
    assert_eq!(fixture.read("b.rs").unwrap(), "fn b() {}\n");

    let target = {
        let session = fixture.lock();
        rewind::target_before_turn(&session, 2).unwrap()
    };
    assert_eq!(target, "s0", "two turns back is the session header");

    let plan = {
        let session = fixture.lock();
        rewind::plan(&session, &target).unwrap()
    };
    assert_eq!(plan.files.len(), 2, "{:?}", plan.files);
    let report = {
        let mut session = fixture.lock();
        rewind::apply(&mut session, &plan, false).unwrap()
    };
    assert_eq!(report.restored, ["a.rs"]);
    assert_eq!(report.deleted, ["b.rs"]);
    assert_eq!(fixture.read("a.rs").unwrap(), "let a = 1;\n");
    assert!(fixture.read("b.rs").is_none());
    assert_eq!(
        fixture.read("outside.txt").unwrap(),
        "mine\n",
        "a file the harness never wrote must not be touched"
    );

    let session = fixture.lock();
    let last_entry = session.tree.last().unwrap().id.clone();
    assert_eq!(
        session.active_leaf_id().as_deref(),
        Some(last_entry.as_str())
    );
    let ids: Vec<String> = session
        .context_path()
        .iter()
        .map(|entry| entry.id.clone())
        .collect();
    assert!(
        !ids.contains(&first_user),
        "the rewritten branch is not in context: {ids:?}"
    );
    // The abandoned branch is still in the tree: both user turns and the old
    // leaf remain reachable.
    assert!(session.tree.get(&first_user).is_some());
    assert!(session.tree.get(&first_leaf).is_some());
    assert!(
        !session.tree.children(&first_user).is_empty(),
        "the abandoned turn keeps its children"
    );
    let rows = localharness::ui::tree::TreeView::new(&session).rows;
    assert!(
        rows.iter().any(|row| row.id == first_assistant),
        "the abandoned branch stays visible in /tree: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.preview == "edit b"),
        "the abandoned user turn is visible: {rows:?}"
    );
    let rewind_entry = session
        .latest_custom_on_path("rewind")
        .expect("rewind marker on the new branch");
    match &rewind_entry.kind {
        EntryKind::Custom(custom) => {
            assert_eq!(custom.payload["target"], "s0");
            assert_eq!(custom.payload["restored"][0], "a.rs");
            assert_eq!(custom.payload["deleted"][0], "b.rs");
        }
        _ => panic!("wrong entry type"),
    }
}

#[tokio::test]
async fn an_external_edit_refuses_and_names_the_file() {
    let fixture = Fixture::new();
    fixture.write("a.rs", "one\n");
    let _ = ReadTool.call(&fixture.ctx, json!({"path": "a.rs"})).await;
    fixture.user("edit");
    fixture
        .mutate(
            &EditTool,
            json!({"path": "a.rs", "old": "one", "new": "two"}),
        )
        .await;
    let leaf = fixture.lock().active_leaf_id().unwrap();
    // The user (or a formatter, or bash) edits afterwards.
    fixture.write("a.rs", "two, and my work\n");

    let target = {
        let session = fixture.lock();
        rewind::target_before_turn(&session, 1).unwrap()
    };
    let plan = {
        let session = fixture.lock();
        rewind::plan(&session, &target).unwrap()
    };
    let error = {
        let mut session = fixture.lock();
        rewind::apply(&mut session, &plan, false).unwrap_err()
    };
    let message = format!("{error:#}");
    assert!(message.contains("a.rs"), "{message}");
    assert!(message.contains("changed outside the harness"), "{message}");
    assert_eq!(fixture.read("a.rs").unwrap(), "two, and my work\n");
    assert_eq!(
        fixture.lock().active_leaf_id().unwrap(),
        leaf,
        "a refused rewind must not move the leaf"
    );

    // --force writes a backup first, then restores.
    let report = {
        let mut session = fixture.lock();
        rewind::apply(&mut session, &plan, true).unwrap()
    };
    assert_eq!(report.backups.len(), 1);
    let backup = fixture.dir.join(&report.backups[0]);
    assert_eq!(
        std::fs::read_to_string(&backup).unwrap(),
        "two, and my work\n"
    );
    assert_eq!(fixture.read("a.rs").unwrap(), "one\n");
}

#[tokio::test]
async fn rewind_of_a_session_without_file_changes_is_a_pure_checkout() {
    let fixture = Fixture::new();
    let first = fixture.user("hello");
    fixture.assistant();
    fixture.user("again");

    let (plan, report) = {
        let mut session = fixture.lock();
        let target = rewind::target_before_turn(&session, 1).unwrap();
        let plan = rewind::plan(&session, &target).unwrap();
        assert!(plan.is_pure_checkout());
        let report = rewind::apply(&mut session, &plan, false).unwrap();
        (plan, report)
    };
    assert!(report.restored.is_empty());
    assert!(report.deleted.is_empty());
    let session = fixture.lock();
    let ids: Vec<String> = session
        .context_path()
        .iter()
        .map(|entry| entry.id.clone())
        .collect();
    assert_eq!(plan.target, "e2");
    assert!(ids.iter().any(|id| id == &first), "{ids:?}");
    assert!(
        !ids.iter().any(|id| id == "e3"),
        "the abandoned user message is gone: {ids:?}"
    );
}

#[test]
fn synthetic_continuation_prompts_are_not_rewind_turns() {
    let fixture = Fixture::new();
    let real = fixture.user("real request");
    fixture.assistant();
    fixture.continuation("[localharness] review nudge: call the review tool");
    fixture.assistant();

    let session = fixture.lock();
    assert_eq!(
        rewind::target_before_turn(&session, 1).unwrap(),
        "s0",
        "the injected continuation is not the last user turn"
    );
    let error = rewind::target_before_turn(&session, 2).unwrap_err();
    assert!(error.contains("1 user message"), "{error}");
    assert_eq!(session.tree.get(&real).unwrap().id, real);
}

#[tokio::test]
async fn a_failed_multi_file_rewind_rolls_back_earlier_files() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    fixture.write("a.rs", "let a = 1;\n");
    std::fs::create_dir_all(fixture.dir.join("sub")).unwrap();
    fixture.write("sub/b.rs", "let b = 1;\n");
    ReadTool.call(&fixture.ctx, json!({"path": "a.rs"})).await;
    ReadTool
        .call(&fixture.ctx, json!({"path": "sub/b.rs"}))
        .await;

    fixture.user("edit both");
    fixture.assistant();
    fixture
        .mutate(&EditTool, json!({"path": "a.rs", "old": "1", "new": "2"}))
        .await;
    fixture
        .mutate(
            &EditTool,
            json!({"path": "sub/b.rs", "old": "1", "new": "2"}),
        )
        .await;
    let leaf = fixture.lock().active_leaf_id().unwrap();

    let plan = {
        let session = fixture.lock();
        let target = rewind::target_before_turn(&session, 1).unwrap();
        rewind::plan(&session, &target).unwrap()
    };
    assert_eq!(plan.files[0].display, "a.rs", "first file is the restore");
    assert_eq!(plan.files.len(), 2);

    // The second restore cannot create its temporary file.
    let sub = fixture.dir.join("sub");
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o555)).unwrap();
    let error = {
        let mut session = fixture.lock();
        rewind::apply(&mut session, &plan, false).unwrap_err()
    };
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();

    let message = format!("{error:#}");
    assert!(message.contains("rolled back 1 earlier file"), "{message}");
    assert_eq!(fixture.read("a.rs").as_deref(), Some("let a = 2;\n"));
    assert_eq!(fixture.read("sub/b.rs").as_deref(), Some("let b = 2;\n"));
    assert_eq!(
        fixture.lock().active_leaf_id().unwrap(),
        leaf,
        "the conversation did not move"
    );
}
