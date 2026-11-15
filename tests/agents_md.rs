//! End-to-end coverage for the hierarchical AGENTS.md loader: repository-root
//! discovery, source order, caps, symlink confinement, and the
//! `lh context --show` manifest.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::{Digest, Sha256};

struct Env {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    config_dir: PathBuf,
    data_dir: PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let config_dir = dir.path().join("config");
    let data_dir = dir.path().join("data");
    Env {
        _dir: dir,
        repo,
        config_dir,
        data_dir,
    }
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

fn init_git(root: &Path) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["init", "-q"])
        .output()
        .expect("run git init");
    assert!(output.status.success(), "{output:?}");
}

fn run(env: &Env, cwd: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lh"));
    command
        .args(args)
        .current_dir(cwd)
        .env("LH_CONFIG_DIR", &env.config_dir)
        .env("LH_DATA_DIR", &env.data_dir)
        .env_remove("LH_AGENTS_MD")
        .env_remove("LH_ENDPOINT")
        .env_remove("LH_MODEL")
        .env_remove("LH_PROFILE")
        .env_remove("LH_TRUST");
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.output().expect("run lh")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[test]
fn context_show_lists_the_monorepo_chain_in_order_with_hashes() {
    let env = env();
    init_git(&env.repo);
    write(&env.repo.join("AGENTS.md"), "root rule\n");
    write(&env.repo.join("pkg/AGENTS.md"), "package rule\n");
    write(&env.repo.join("pkg/leaf/AGENTS.md"), "leaf rule\n");

    let output = run(
        &env,
        &env.repo.join("pkg/leaf"),
        &["context", "--show"],
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);

    let root_at = text.find(" AGENTS.md  ").expect("root line");
    let package_at = text.find(" pkg/AGENTS.md  ").expect("package line");
    let leaf_at = text.find(" pkg/leaf/AGENTS.md  ").expect("leaf line");
    assert!(
        root_at < package_at && package_at < leaf_at,
        "source order:\n{text}"
    );
    assert!(text.contains("loaded: 3 files"), "{text}");
    assert!(text.contains(&sha256(b"root rule\n")), "{text}");
    assert!(text.contains(&sha256(b"package rule\n")), "{text}");
    assert!(text.contains(&sha256(b"leaf rule\n")), "{text}");
    assert!(
        !env.data_dir.join("sessions").exists(),
        "lh context must not open a session"
    );

    let output = run(
        &env,
        &env.repo.join("pkg/leaf"),
        &["context", "--json"],
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let manifest: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    let sources: Vec<&str> = manifest["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|file| file["source"].as_str().expect("source"))
        .collect();
    assert_eq!(
        sources,
        vec!["AGENTS.md", "pkg/AGENTS.md", "pkg/leaf/AGENTS.md"]
    );
    assert!(
        manifest["files"][0]["sha256"]
            .as_str()
            .unwrap()
            .eq(&sha256(b"root rule\n"))
    );
}

#[test]
fn context_show_truncates_at_the_total_cap_deterministically_with_a_warning() {
    use localharness::agent::agents_md::{PER_FILE_CAP_BYTES, TOTAL_CAP_BYTES};

    let env = env();
    init_git(&env.repo);
    let root_body = "a".repeat(PER_FILE_CAP_BYTES);
    let package_body = "b".repeat(PER_FILE_CAP_BYTES);
    let leaf_body = "c".repeat(PER_FILE_CAP_BYTES);
    write(&env.repo.join("AGENTS.md"), &root_body);
    write(&env.repo.join("pkg/AGENTS.md"), &package_body);
    write(&env.repo.join("pkg/leaf/AGENTS.md"), &leaf_body);

    let cwd = env.repo.join("pkg/leaf");
    let first = run(&env, &cwd, &["context", "--show"], &[]);
    let second = run(&env, &cwd, &["context", "--show"], &[]);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    assert_eq!(
        stdout(&first),
        stdout(&second),
        "truncation is deterministic"
    );
    assert_eq!(
        stderr(&first),
        stderr(&second),
        "warnings are deterministic"
    );

    let text = stdout(&first);
    assert!(
        text.contains(&format!("loaded: 3 files, {TOTAL_CAP_BYTES} bytes")),
        "{text}"
    );
    assert!(text.contains("truncated AGENTS.md"), "{text}");
    let root_keep = TOTAL_CAP_BYTES - 2 * PER_FILE_CAP_BYTES;
    assert!(
        text.contains(&sha256(&root_body.as_bytes()[..root_keep])),
        "the root file keeps exactly the remaining {root_keep} bytes:\n{text}"
    );
    assert!(text.contains(&sha256(package_body.as_bytes())), "{text}");
    assert!(text.contains(&sha256(leaf_body.as_bytes())), "{text}");

    let warnings = stderr(&first);
    assert!(warnings.contains("total cap"), "{warnings}");
    assert!(
        warnings.contains(&TOTAL_CAP_BYTES.to_string()),
        "{warnings}"
    );
}

#[test]
fn context_show_refuses_a_symlink_outside_the_repository() {
    let env = env();
    init_git(&env.repo);
    let outside = env._dir.path().join("outside.md");
    write(&outside, "outside rule\n");
    std::os::unix::fs::symlink(&outside, env.repo.join("AGENTS.md")).unwrap();

    let output = run(&env, &env.repo, &["context", "--show"], &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("skipped"), "{text}");
    assert!(text.contains("outside the repository"), "{text}");
    assert!(!text.contains("sha256:"), "{text}");
    let warnings = stderr(&output);
    assert!(warnings.contains("outside the repository"), "{warnings}");
}

#[test]
fn no_agents_md_flag_env_and_config_disable_loading() {
    let env = env();
    init_git(&env.repo);
    write(&env.repo.join("AGENTS.md"), "root rule\n");

    let loaded = run(&env, &env.repo, &["context", "--show"], &[]);
    assert_eq!(loaded.status.code(), Some(0), "{}", stderr(&loaded));
    assert!(stdout(&loaded).contains("sha256:"), "{}", stdout(&loaded));

    let flagged = run(
        &env,
        &env.repo,
        &["--no-agents-md", "context", "--show"],
        &[],
    );
    assert_eq!(flagged.status.code(), Some(0), "{}", stderr(&flagged));
    assert!(
        stdout(&flagged).contains("disabled"),
        "{}",
        stdout(&flagged)
    );
    assert!(
        !stdout(&flagged).contains("sha256:"),
        "{}",
        stdout(&flagged)
    );

    let from_env = run(
        &env,
        &env.repo,
        &["context", "--show"],
        &[("LH_AGENTS_MD", "0")],
    );
    assert_eq!(from_env.status.code(), Some(0), "{}", stderr(&from_env));
    assert!(
        stdout(&from_env).contains("disabled"),
        "{}",
        stdout(&from_env)
    );

    std::fs::create_dir_all(&env.config_dir).unwrap();
    std::fs::write(
        env.config_dir.join("config.toml"),
        "[prompt]\nagents_md = false\n",
    )
    .unwrap();
    let from_config = run(&env, &env.repo, &["context", "--show"], &[]);
    assert_eq!(
        from_config.status.code(),
        Some(0),
        "{}",
        stderr(&from_config)
    );
    assert!(
        stdout(&from_config).contains("disabled"),
        "{}",
        stdout(&from_config)
    );
}
