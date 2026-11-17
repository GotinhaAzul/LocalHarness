//! Skill specification, package manager, lint, and eval end to end.
//!
//! Every test uses temp directories, a temporary HOME, and a scripted local
//! endpoint or no endpoint at all. Git fixtures are created in the test; no
//! network is used.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use bytes::Bytes;
use futures_util::stream;
use serde_json::Value;

#[derive(Clone, Default)]
struct Fake {
    scripts: Arc<Mutex<VecDeque<String>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn chat(State(fake): State<Fake>, body: String) -> Response {
    fake.requests
        .lock()
        .unwrap()
        .push(serde_json::from_str(&body).unwrap_or_default());
    let frames = fake.scripts.lock().unwrap().pop_front().unwrap_or_else(|| {
        concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        )
        .to_string()
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        )
        .body(Body::from_stream(stream::iter(
            frames
                .as_bytes()
                .chunks(9)
                .map(|chunk| Ok::<Bytes, Infallible>(Bytes::copy_from_slice(chunk)))
                .collect::<Vec<_>>(),
        )))
        .unwrap()
}

async fn models() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )
        .body(Body::from(
            r#"{"object":"list","data":[{"id":"test-model","object":"model"}]}"#,
        ))
        .unwrap()
}

async fn spawn_fake(scripts: Vec<String>) -> (String, Fake) {
    let fake = Fake {
        scripts: Arc::new(Mutex::new(scripts.into())),
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/models", axum::routing::get(models))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), fake)
}

fn text_frame(text: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({"choices": [{"delta": {"content": text}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
    )
}

fn tool_frame(name: &str, arguments: &Value) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call_1",
            "function": {"name": name, "arguments": arguments.to_string()}
        }]}}]}),
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    )
}

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
    config: PathBuf,
    data: PathBuf,
    cwd: PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let config = dir.path().join("config");
    let data = dir.path().join("data");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    Env {
        _dir: dir,
        home,
        config,
        data,
        cwd,
    }
}

fn apply_env(command: &mut tokio::process::Command, env: &Env) {
    command
        .current_dir(&env.cwd)
        .env("HOME", &env.home)
        .env("TOT_HOME", &env.home)
        .env("TOT_CONFIG_DIR", &env.config)
        .env("TOT_DATA_DIR", &env.data)
        .env("TOT_SKIP_TOOL_PROBE", "1")
        .env_remove("TOT_ENDPOINT")
        .env_remove("LH_ENDPOINT")
        .env_remove("TOT_MODEL")
        .env_remove("LH_MODEL")
        .env_remove("TOT_API_KEY")
        .env_remove("LH_API_KEY")
        .env_remove("TOT_PROFILE")
        .env_remove("LH_PROFILE")
        .env_remove("TOT_TRUST")
        .env_remove("LH_TRUST");
}

async fn run(env: &Env, args: &[&str]) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_tot"));
    command.args(args);
    apply_env(&mut command, env);
    tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
        .await
        .expect("tot timed out")
        .expect("cannot run tot")
}

async fn run_print(env: &Env, base: &str, args: &[&str]) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_tot"));
    command
        .arg("--endpoint")
        .arg(base)
        .arg("--model")
        .arg("test-model")
        .arg("--no-session")
        .args(args);
    apply_env(&mut command, env);
    tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
        .await
        .expect("tot timed out")
        .expect("cannot run tot")
}

fn write_skill(root: &Path, name: &str, description: &str, body: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
    )
    .unwrap();
}

fn write_raw_skill(root: &Path, name: &str, frontmatter: &str, body: &str) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\n{frontmatter}\n---\n\n{body}\n"),
    )
    .unwrap();
    dir
}

fn json(output: &std::process::Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("not JSON: {error}\nstdout: {stdout}"))
}

fn assert_ok(output: &std::process::Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
}

#[tokio::test]
async fn a_claude_skills_fixture_loads_without_conversion_and_the_body_stays_back() {
    let env = env();
    let skill = env.cwd.join(".claude/skills/review");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: review\ndescription: PROJECT-ONLY-DESCRIPTION\n---\n\nBODY-SENTINEL review instructions\n",
    )
    .unwrap();
    let (base, fake) = spawn_fake(vec![text_frame("ok")]).await;

    let output = run_print(&env, &base, &["--trust", "--print", "hello"]).await;
    assert_ok(&output);
    {
        let requests = fake.requests.lock().unwrap();
        let system = requests[0]["messages"][0]["content"]
            .as_str()
            .expect("system prompt");
        assert!(
            system.contains("review: PROJECT-ONLY-DESCRIPTION"),
            "{system}"
        );
        assert!(!system.contains("BODY-SENTINEL"), "body reached the prompt");
    }

    // Project standard directories are repository content: no trust, no load.
    let (base, fake) = spawn_fake(vec![text_frame("ok")]).await;
    let output = run_print(&env, &base, &["--print", "hello"]).await;
    assert_ok(&output);
    let requests = fake.requests.lock().unwrap();
    let system = requests[0]["messages"][0]["content"]
        .as_str()
        .expect("system prompt");
    assert!(!system.contains("PROJECT-ONLY-DESCRIPTION"), "{system}");
}

#[tokio::test]
async fn invalid_names_and_long_descriptions_are_field_errors() {
    let env = env();
    let skills = env.config.join("skills");
    write_raw_skill(
        &skills,
        "bad-name-dir",
        &format!("name: Bad_Name\ndescription: {}", "x".repeat(1_025)),
        "Body",
    );
    let output = run(&env, &["skill", "lint", "--json"]).await;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let value = json(&output);
    let diagnostics = value["skills"][0]["diagnostics"].as_array().unwrap();
    let fields: Vec<&str> = diagnostics
        .iter()
        .filter_map(|diagnostic| diagnostic["field"].as_str())
        .collect();
    assert!(fields.contains(&"name"), "{diagnostics:?}");
    assert!(fields.contains(&"description"), "{diagnostics:?}");
    assert!(value["errors"].as_u64().unwrap() >= 2);
}

#[tokio::test]
async fn a_hundred_skills_are_capped_and_stay_loadable_by_name() {
    let env = env();
    let skills = env.config.join("skills");
    for index in 0..100 {
        write_skill(
            &skills,
            &format!("skill-{index}"),
            &format!("Capability number {index}"),
            &format!("BODY-SENTINEL-{index}"),
        );
    }
    let (base, fake) = spawn_fake(vec![
        tool_frame("skill", &serde_json::json!({"name": "skill-99"})),
        text_frame("loaded"),
    ])
    .await;
    let output = run_print(&env, &base, &["--print", "hello"]).await;
    assert_ok(&output);
    let requests = fake.requests.lock().unwrap();
    let system = requests[0]["messages"][0]["content"]
        .as_str()
        .expect("system prompt");
    // Full entries stop at the total cap. The trailing line accounts for every
    // remaining skill: shown by name, or counted in `and N more` when even the
    // names cannot fit.
    assert!(
        system.matches("Capability number").count() < 100,
        "the index did not stop writing full entries: {system}"
    );
    let tail = system
        .lines()
        .find(|line| line.starts_with("- more (load with the skill tool by name):"))
        .unwrap_or_else(|| panic!("no names line:\n{system}"));
    assert!(
        tail.chars().count() <= totality::skills::INDEX_NAMES_CAP,
        "{tail}"
    );
    let (mut shown, mut omitted) = (0usize, 0usize);
    for item in tail.split(": ").nth(1).unwrap().split(", ") {
        if let Some(count) = item
            .strip_prefix("and ")
            .and_then(|rest| rest.strip_suffix(" more"))
        {
            omitted = count.parse().unwrap();
        } else {
            shown += 1;
        }
    }
    let full = system
        .lines()
        .filter(|line| line.starts_with("- skill-") && line.contains(": Capability number"))
        .count();
    assert_eq!(full + shown + omitted, 100, "{system}");
    assert!(
        !system.contains("BODY-SENTINEL"),
        "a body reached the prompt"
    );
    assert!(system.len() < 40_000, "index is {} chars", system.len());

    // A skill whose description was left out of the prompt still loads by
    // name: the tool resolves the full index, not the clipped text.
    let messages = requests[1]["messages"].as_array().unwrap();
    let tool = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("tool result");
    let content = tool["content"].as_str().unwrap();
    assert!(
        content.contains("BODY-SENTINEL-99"),
        "skill-99 did not load: {content}"
    );
}

#[tokio::test]
async fn allowed_tools_cannot_enable_a_tool_outside_the_profile() {
    let env = env();
    std::fs::write(
        env.config.join("config.toml"),
        "[profile.readonly]\ntools = [\"read\", \"skill\"]\n",
    )
    .unwrap();
    write_raw_skill(
        &env.config.join("skills"),
        "bash-helper",
        "name: bash-helper\ndescription: Helper that claims bash\nallowed-tools: bash",
        "Body",
    );
    let (base, fake) = spawn_fake(vec![
        tool_frame(
            "bash",
            &serde_json::json!({"command": "touch expanded.txt"}),
        ),
        text_frame("stopped"),
    ])
    .await;
    let output = run_print(
        &env,
        &base,
        &["--profile", "readonly", "--print", "run bash"],
    )
    .await;
    assert_ok(&output);
    let requests = fake.requests.lock().unwrap();
    let tools = requests[0]["tools"].as_array().expect("tool schema array");
    assert!(
        !tools.iter().any(|tool| tool["function"]["name"] == "bash"),
        "allowed-tools expanded the schema: {tools:?}"
    );
    let messages = requests[1]["messages"].as_array().unwrap();
    let tool = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("tool result");
    let content = tool["content"].as_str().unwrap();
    assert!(
        content.contains("unknown tool") || content.contains("not available"),
        "tool result: {content}"
    );
    assert!(!env.cwd.join("expanded.txt").exists());
}

#[tokio::test]
async fn native_and_standard_skills_resolve_deterministically_and_record_the_shadow() {
    let env = env();
    write_skill(
        &env.home.join(".claude/skills"),
        "deploy",
        "Claude description",
        "claude body",
    );
    write_skill(
        &env.config.join("skills"),
        "deploy",
        "Native description",
        "native body",
    );
    let (base, fake) = spawn_fake(vec![text_frame("ok")]).await;
    let output = run_print(&env, &base, &["--print", "hello"]).await;
    assert_ok(&output);
    {
        let requests = fake.requests.lock().unwrap();
        let system = requests[0]["messages"][0]["content"]
            .as_str()
            .expect("system prompt");
        assert!(system.contains("Native description"), "{system}");
        assert!(!system.contains("Claude description"), "{system}");
    }

    // The duplicate is visible to lint and names both origins.
    let output = run(&env, &["skill", "lint", "--all", "--json"]).await;
    assert_eq!(output.status.code(), Some(1));
    let value = json(&output);
    let duplicates = value["duplicates"].as_array().unwrap();
    assert_eq!(duplicates.len(), 1, "{value}");
    assert_eq!(duplicates[0]["code"], "skill_duplicate_name");
    let message = duplicates[0]["message"].as_str().unwrap();
    assert!(message.contains(".claude/skills"), "{message}");
    assert!(message.contains("~/.config/totality/skills"), "{message}");
}

#[tokio::test]
async fn explicit_shadow_makes_a_bundled_override_intentional() {
    let env = env();
    write_raw_skill(
        &env.config.join("skills"),
        "test-repair",
        "name: test-repair\ndescription: My variant\nshadow: true",
        "Variant body",
    );
    let output = run(&env, &["skill", "lint", "--all", "--json"]).await;
    assert_ok(&output);
    let value = json(&output);
    assert_eq!(value["errors"], 0, "{value}");
    let duplicates = value["duplicates"].as_array().unwrap();
    assert_eq!(duplicates.len(), 1, "{value}");
    assert_eq!(duplicates[0]["code"], "skill_shadow_explicit");
    assert_eq!(duplicates[0]["severity"], "warning");
}

#[tokio::test]
async fn installing_from_a_directory_and_a_git_ref_writes_a_lockfile() {
    let env = env();
    let source = env._dir.path().join("source/deploy");
    std::fs::create_dir_all(source.join("references")).unwrap();
    std::fs::write(
        source.join("SKILL.md"),
        "---\nname: deploy\ndescription: Deploy the service\n---\n\nBody\n",
    )
    .unwrap();
    std::fs::write(source.join("references/rollback.md"), "Roll back\n").unwrap();

    let output = run(&env, &["skill", "add", source.to_str().unwrap()]).await;
    assert_ok(&output);
    let lock = std::fs::read_to_string(env.config.join("skills.lock")).unwrap();
    assert!(lock.contains("name = \"deploy\""), "{lock}");
    assert!(lock.contains("source = "), "{lock}");
    assert!(lock.contains("installed_at = "), "{lock}");
    assert!(lock.contains("[skill.files]"), "{lock}");
    assert!(lock.contains("\"SKILL.md\" = "), "{lock}");

    // A local git repository, checked out at a named branch.
    let repo = env._dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("SKILL.md"),
        "---\nname: git-skill\ndescription: From git\n---\n\nGit body\n",
    )
    .unwrap();
    git(&repo, &["init", "--quiet"]);
    git(&repo, &["checkout", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    git(&repo, &["add", "SKILL.md"]);
    git(&repo, &["commit", "-q", "-m", "skill"]);
    let commit = git_output(&repo, &["rev-parse", "HEAD"]);

    let source_arg = format!("{}#main", repo.display());
    let output = run(&env, &["skill", "add", &source_arg]).await;
    assert_ok(&output);
    let list = run(&env, &["skill", "list", "--json"]).await;
    assert_ok(&list);
    let value = json(&list);
    let git_skill = value["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|skill| skill["name"] == "git-skill")
        .expect("git skill");
    assert_eq!(git_skill["ref"], "main");
    assert_eq!(git_skill["pinned_ref"], commit);
    assert_eq!(git_skill["source"].as_str().unwrap(), source_arg.as_str());
    assert_eq!(git_skill["digest"].as_str().unwrap().len(), 64);
    assert!(git_skill["files"]["SKILL.md"].is_string());

    // A new commit on the same ref updates the installed copy and the pin.
    std::fs::write(
        repo.join("SKILL.md"),
        "---\nname: git-skill\ndescription: From git\n---\n\nGit body v2\n",
    )
    .unwrap();
    git(&repo, &["add", "SKILL.md"]);
    git(&repo, &["commit", "-q", "-m", "update skill"]);
    let second = git_output(&repo, &["rev-parse", "HEAD"]);
    let output = run(&env, &["skill", "update", "git-skill"]).await;
    assert_ok(&output);
    assert!(
        std::fs::read_to_string(env.config.join("skills/git-skill/SKILL.md"))
            .unwrap()
            .contains("Git body v2")
    );
    let list = run(&env, &["skill", "list", "--json"]).await;
    assert_ok(&list);
    let value = json(&list);
    let git_skill = value["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|skill| skill["name"] == "git-skill")
        .expect("git skill");
    assert_eq!(git_skill["pinned_ref"], second);
    assert_ne!(git_skill["pinned_ref"], commit);
}

#[tokio::test]
async fn frozen_detects_a_changed_file_and_changes_nothing() {
    let env = env();
    let source = env._dir.path().join("source/deploy");
    write_skill(source.parent().unwrap(), "deploy", "Deploy", "original");
    let output = run(&env, &["skill", "add", source.to_str().unwrap()]).await;
    assert_ok(&output);
    let installed = env.config.join("skills/deploy/SKILL.md");
    std::fs::write(
        &installed,
        "---\nname: deploy\ndescription: Deploy\n---\n\nedited\n",
    )
    .unwrap();

    let output = run(&env, &["skill", "list", "--frozen", "--json"]).await;
    assert_eq!(output.status.code(), Some(1));
    let value = json(&output);
    assert_eq!(value["drift"][0]["file"], "SKILL.md");
    assert!(
        value["drift"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("digest")
    );

    let output = run(&env, &["skill", "update", "--frozen"]).await;
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("skills.lock drift detected"), "{stderr}");
    assert!(
        std::fs::read_to_string(&installed)
            .unwrap()
            .contains("edited"),
        "frozen changed the installed copy"
    );
}

#[tokio::test]
async fn update_refuses_local_edits_and_force_backs_up_the_old_copy() {
    let env = env();
    let source = env._dir.path().join("source/deploy");
    write_skill(source.parent().unwrap(), "deploy", "Deploy", "original");
    let output = run(&env, &["skill", "add", source.to_str().unwrap()]).await;
    assert_ok(&output);
    let installed = env.config.join("skills/deploy/SKILL.md");
    std::fs::write(
        &installed,
        "---\nname: deploy\ndescription: Deploy\n---\n\nLOCAL-EDIT\n",
    )
    .unwrap();

    let output = run(&env, &["skill", "update"]).await;
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("drift detected"), "{stderr}");
    assert!(
        std::fs::read_to_string(&installed)
            .unwrap()
            .contains("LOCAL-EDIT")
    );

    // Change the source too, then force: the edited copy moves to trash.
    std::fs::write(
        source.join("SKILL.md"),
        "---\nname: deploy\ndescription: Deploy\n---\n\nORIGINAL-V2\n",
    )
    .unwrap();
    let output = run(&env, &["skill", "update", "--force"]).await;
    assert_ok(&output);
    assert!(
        std::fs::read_to_string(&installed)
            .unwrap()
            .contains("ORIGINAL-V2")
    );
    let trash = trash_files(&env.config);
    assert!(
        trash.iter().any(|path| std::fs::read_to_string(path)
            .unwrap()
            .contains("LOCAL-EDIT")),
        "the edited copy was not backed up: {trash:?}"
    );
}

#[tokio::test]
async fn install_never_runs_a_bundled_script_and_execution_needs_approval() {
    let env = env();
    let source = env._dir.path().join("source/scripted");
    std::fs::create_dir_all(source.join("scripts")).unwrap();
    std::fs::write(
        source.join("SKILL.md"),
        "---\nname: scripted\ndescription: Ships a script\n---\n\nBody\n",
    )
    .unwrap();
    std::fs::write(
        source.join("scripts/setup.sh"),
        format!(
            "#!/bin/sh\ntouch {}\n",
            env.cwd.join("ran-by-install").display()
        ),
    )
    .unwrap();
    let output = run(&env, &["skill", "add", source.to_str().unwrap()]).await;
    assert_ok(&output);
    assert!(
        !env.cwd.join("ran-by-install").exists(),
        "install executed a bundled script"
    );

    let script = env.config.join("skills/scripted/scripts/setup.sh");
    let (base, fake) = spawn_fake(vec![
        tool_frame(
            "bash",
            &serde_json::json!({"command": script.display().to_string()}),
        ),
        text_frame("stopped"),
    ])
    .await;
    let output = run_print(&env, &base, &["--print", "run the skill script"]).await;
    assert_ok(&output);
    assert!(
        !env.cwd.join("ran-by-install").exists(),
        "a bundled script ran without approval"
    );
    let requests = fake.requests.lock().unwrap();
    let messages = requests[1]["messages"].as_array().unwrap();
    let tool = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("tool result");
    let content = tool["content"].as_str().unwrap();
    assert!(content.contains("SKILL_SCRIPT_APPROVAL"), "{content}");
    assert!(content.contains("setup.sh"), "{content}");
}

#[tokio::test]
async fn removing_a_skill_backs_it_up_and_the_session_still_starts() {
    let env = env();
    let source = env._dir.path().join("source/deploy");
    write_skill(source.parent().unwrap(), "deploy", "Deploy", "body");
    assert_ok(&run(&env, &["skill", "add", source.to_str().unwrap()]).await);
    let output = run(&env, &["skill", "remove", "deploy"]).await;
    assert_ok(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("moved to"), "{stdout}");
    assert!(!env.config.join("skills/deploy").exists());
    assert!(
        trash_files(&env.config)
            .iter()
            .any(|path| std::fs::read_to_string(path).unwrap().contains("body")),
        "remove deleted the copy instead of trashing it"
    );

    let list = run(&env, &["skill", "list", "--json"]).await;
    assert_ok(&list);
    let value = json(&list);
    assert!(
        !value["skills"]
            .as_array()
            .unwrap()
            .iter()
            .any(|skill| skill["name"] == "deploy")
    );

    let output = run(&env, &["skill", "remove", "deploy"]).await;
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not installed"), "{stderr}");

    let (base, _fake) = spawn_fake(vec![text_frame("session started")]).await;
    let output = run_print(&env, &base, &["--print", "hello"]).await;
    assert_ok(&output);
}

#[tokio::test]
async fn lint_fails_a_missing_reference_and_a_duplicate_name() {
    let env = env();
    write_raw_skill(
        &env.config.join("skills"),
        "broken",
        "name: broken\ndescription: Broken skill",
        "See references/missing.md for details.",
    );
    write_skill(
        &env.config.join("skills"),
        "duplicate",
        "Global duplicate",
        "one",
    );
    write_skill(
        &env.cwd.join(".harness/skills"),
        "duplicate",
        "Project duplicate",
        "two",
    );
    let output = run(&env, &["--trust", "skill", "lint", "--all", "--json"]).await;
    assert_eq!(output.status.code(), Some(1));
    let value = json(&output);
    let mut codes = Vec::new();
    for skill in value["skills"].as_array().unwrap() {
        for diagnostic in skill["diagnostics"].as_array().unwrap() {
            codes.push(diagnostic["code"].as_str().unwrap().to_string());
        }
    }
    for diagnostic in value["duplicates"].as_array().unwrap() {
        codes.push(diagnostic["code"].as_str().unwrap().to_string());
    }
    assert!(
        codes.contains(&"skill_reference_missing".to_string()),
        "{codes:?}"
    );
    assert!(
        codes.contains(&"skill_duplicate_name".to_string()),
        "{codes:?}"
    );
}

#[tokio::test]
async fn lint_warns_on_a_long_body_and_many_references() {
    let env = env();
    let skill = env.config.join("skills/large");
    std::fs::create_dir_all(skill.join("references")).unwrap();
    let mut body = String::from("Body\n");
    for line in 0..400 {
        body.push_str(&format!("line {line}\n"));
    }
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: large\ndescription: Large skill\n---\n\n{body}\n"),
    )
    .unwrap();
    for reference in 0..11 {
        std::fs::write(
            skill.join(format!("references/note-{reference}.md")),
            "note\n",
        )
        .unwrap();
    }
    let output = run(&env, &["skill", "lint", "large", "--json"]).await;
    assert_ok(&output);
    let value = json(&output);
    let diagnostics = value["skills"][0]["diagnostics"].as_array().unwrap();
    let codes: Vec<&str> = diagnostics
        .iter()
        .filter_map(|diagnostic| diagnostic["code"].as_str())
        .collect();
    assert!(codes.contains(&"skill_body_long"), "{codes:?}");
    assert!(codes.contains(&"skill_references_many"), "{codes:?}");
    assert!(value["warnings"].as_u64().unwrap() >= 2);
}

#[tokio::test]
async fn lint_warns_when_an_entry_would_be_clipped_in_the_index() {
    let env = env();
    write_skill(
        &env.config.join("skills"),
        "wordy",
        &"Trigger word ".repeat(40),
        "Body",
    );
    let output = run(&env, &["skill", "lint", "wordy", "--json"]).await;
    assert_ok(&output);
    let value = json(&output);
    let diagnostics = value["skills"][0]["diagnostics"].as_array().unwrap();
    let clipped = diagnostics
        .iter()
        .find(|diagnostic| diagnostic["code"] == "skill_index_clipped")
        .unwrap_or_else(|| panic!("no index warning: {diagnostics:?}"));
    assert_eq!(clipped["severity"], "warning");
    assert_eq!(clipped["field"], "description");
    let message = clipped["message"].as_str().unwrap();
    assert!(message.contains("Shorten the description"), "{message}");
    assert!(message.contains("200"), "{message}");

    // A short entry raises no index warning.
    write_skill(
        &env.config.join("skills"),
        "brief",
        "A short trigger",
        "Body",
    );
    let output = run(&env, &["skill", "lint", "brief", "--json"]).await;
    assert_ok(&output);
    let value = json(&output);
    let diagnostics = value["skills"][0]["diagnostics"].as_array().unwrap();
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic["code"] != "skill_index_clipped"),
        "{diagnostics:?}"
    );
}

#[tokio::test]
async fn lint_all_checks_bundled_global_and_trusted_project_skills() {
    let env = env();
    write_skill(
        &env.config.join("skills"),
        "global-skill",
        "Global skill",
        "global body",
    );
    write_skill(
        &env.cwd.join(".harness/skills"),
        "project-skill",
        "Project skill",
        "project body",
    );
    let output = run(&env, &["--trust", "skill", "lint", "--all", "--json"]).await;
    assert_ok(&output);
    let value = json(&output);
    assert_eq!(value["errors"], 0, "{value}");
    let skills = value["skills"].as_array().unwrap();
    assert_eq!(skills.len(), 13);
    let origins: Vec<&str> = skills
        .iter()
        .filter_map(|skill| skill["origin"].as_str())
        .collect();
    assert!(origins.contains(&"bundled"));
    assert!(origins.contains(&"~/.config/totality/skills"));
    assert!(origins.contains(&".harness/skills"));
}

#[tokio::test]
async fn install_refuses_invalid_metadata_before_copying() {
    let env = env();
    let source = env._dir.path().join("source/bad");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(
        source.join("SKILL.md"),
        format!(
            "---\nname: Bad_Name\ndescription: {}\n---\n\nBody\n",
            "x".repeat(1_025)
        ),
    )
    .unwrap();
    let output = run(&env, &["skill", "add", source.to_str().unwrap()]).await;
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("[name]"), "{stderr}");
    assert!(stderr.contains("[description]"), "{stderr}");
    assert!(!env.config.join("skills/Bad_Name").exists());
    assert!(!env.config.join("skills.lock").exists());
}

#[tokio::test]
async fn lint_rejects_frontmatter_that_claims_tools_or_scope() {
    let env = env();
    write_raw_skill(
        &env.config.join("skills"),
        "grant",
        "name: grant\ndescription: Claims a grant\ntools: bash\nscope: 10.0.0.0/8",
        "Body",
    );
    let output = run(&env, &["skill", "lint", "grant", "--json"]).await;
    assert_eq!(output.status.code(), Some(1));
    let value = json(&output);
    let diagnostics = value["skills"][0]["diagnostics"].as_array().unwrap();
    let codes: Vec<&str> = diagnostics
        .iter()
        .filter_map(|diagnostic| diagnostic["code"].as_str())
        .collect();
    assert_eq!(
        codes
            .iter()
            .filter(|code| **code == "skill_frontmatter_grants")
            .count(),
        2
    );
}

#[tokio::test]
async fn eval_reports_precision_recall_and_token_cost() {
    let env = env();
    let skill = env.config.join("skills/deploy");
    std::fs::create_dir_all(skill.join("references")).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: deploy\ndescription: Deploy the service to production\n---\n\nDeploy instructions.\n",
    )
    .unwrap();
    std::fs::write(
        skill.join("references/rollback.md"),
        "Rollback procedure.\n",
    )
    .unwrap();
    let fixtures = env._dir.path().join("fixtures");
    std::fs::create_dir_all(&fixtures).unwrap();
    std::fs::write(
        fixtures.join("load.toml"),
        "prompt = \"deploy the service to production\"\nskill = \"deploy\"\nexpect = \"load\"\n",
    )
    .unwrap();
    // No `skill` field: the flat directory has exactly one installed skill.
    std::fs::write(
        fixtures.join("skip.toml"),
        "prompt = \"write a haiku about the sea\"\nexpect = \"skip\"\n",
    )
    .unwrap();
    let output = run(
        &env,
        &[
            "skill",
            "eval",
            "--fixtures",
            fixtures.to_str().unwrap(),
            "--json",
        ],
    )
    .await;
    assert_ok(&output);
    let value = json(&output);
    assert_eq!(value["passed"], true);
    assert_eq!(value["false_load_cost"], 0);
    assert!(value["average_loaded_tokens"].as_f64().unwrap() > 0.0);
    assert!(value["total_loaded_tokens"].as_u64().unwrap() > 0);
    assert_eq!(value["skills"][0]["precision"], 1.0);
    assert_eq!(value["skills"][0]["recall"], 1.0);

    // A skip fixture that loads is a failure, not a warning.
    std::fs::write(
        fixtures.join("skip.toml"),
        "prompt = \"deploy the service\"\nskill = \"deploy\"\nexpect = \"skip\"\n",
    )
    .unwrap();
    let output = run(
        &env,
        &[
            "skill",
            "eval",
            "--fixtures",
            fixtures.to_str().unwrap(),
            "--json",
        ],
    )
    .await;
    assert_eq!(output.status.code(), Some(1));
    let value = json(&output);
    assert_eq!(value["passed"], false);
    assert_eq!(value["failures"], 1);
    assert!(value["false_load_cost"].as_u64().unwrap() > 0);

    // A skip fixture for one skill passes when a different skill loads.
    write_skill(
        &env.config.join("skills"),
        "haiku",
        "Write a haiku about nature",
        "Haiku instructions.",
    );
    std::fs::write(
        fixtures.join("skip.toml"),
        "prompt = \"write a haiku about the sea\"\nskill = \"deploy\"\nexpect = \"skip\"\n",
    )
    .unwrap();
    let output = run(
        &env,
        &[
            "skill",
            "eval",
            "--fixtures",
            fixtures.to_str().unwrap(),
            "--json",
        ],
    )
    .await;
    assert_ok(&output);
    let value = json(&output);
    let deploy = value["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|metrics| metrics["skill"] == "deploy")
        .expect("deploy metrics");
    assert_eq!(deploy["true_negatives"], 1, "{value}");
    assert_eq!(deploy["false_positives"], 0, "{value}");
}

#[tokio::test]
async fn a_relative_install_source_is_persisted_as_an_absolute_path() {
    let env = env();
    let source = env._dir.path().join("source/deploy");
    write_skill(source.parent().unwrap(), "deploy", "Deploy", "Body");
    let output = run(&env, &["skill", "add", "../source/deploy"]).await;
    assert_ok(&output);
    let lock: toml::Value =
        toml::from_str(&std::fs::read_to_string(env.config.join("skills.lock")).unwrap()).unwrap();
    let persisted = lock["skill"][0]["source"].as_str().unwrap();
    let canonical = source.canonicalize().unwrap();
    assert_eq!(std::path::Path::new(persisted), canonical);
    assert!(std::path::Path::new(persisted).is_absolute());
}

fn git(cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_output(cwd: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git");
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn trash_files(config: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let root = config.join("skills-trash");
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            collect_files(&entry.path(), &mut files);
        }
    }
    files
}

fn collect_files(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        out.push(path.to_path_buf());
        return;
    }
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            collect_files(&entry.path(), out);
        }
    }
}
