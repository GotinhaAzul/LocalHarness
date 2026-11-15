//! The installer's offline happy path and checksum-mismatch refusal.
//!
//! Each test builds a fake release tarball, points `install.sh` at the local
//! directory through `LH_INSTALL_SOURCE_DIR`, and never touches the network or
//! the real `~/.local/bin`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::{Digest, Sha256};
use tempfile::TempDir;

const VERSION: &str = "0.1.0-test";

fn target() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64-unknown-linux-musl",
        "aarch64" => "aarch64-unknown-linux-musl",
        other => panic!("the installer tests only run on x86_64 or aarch64, not {other}"),
    }
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Build the same tarball layout the release workflow publishes: `lh`,
/// `completions/`, and `man/` at the archive root.
fn make_dist(root: &Path, version: &str, valid_checksum: bool) -> PathBuf {
    fs::create_dir_all(root).unwrap();
    let stage = root.join("stage");
    fs::create_dir_all(stage.join("completions")).unwrap();
    fs::create_dir_all(stage.join("man")).unwrap();

    let binary = stage.join("lh");
    fs::write(
        &binary,
        "#!/bin/sh\nif [ \"${1:-}\" = \"--version\" ]; then\n  printf 'lh 0.1.0-test\\n'\n  exit 0\nfi\nprintf 'fake lh\\n'\n",
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(stage.join("completions/lh.bash"), "# bash completion\n").unwrap();
    fs::write(stage.join("completions/lh.zsh"), "# zsh completion\n").unwrap();
    fs::write(stage.join("completions/lh.fish"), "# fish completion\n").unwrap();
    fs::write(stage.join("man/lh.1"), ".TH lh 1\n").unwrap();

    let artifact_name = format!("lh-v{version}-{}.tar.gz", target());
    let artifact = root.join(&artifact_name);
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&artifact)
        .arg("-C")
        .arg(&stage)
        .arg(".")
        .status()
        .expect("tar is required to build the installer fixture");
    assert!(status.success(), "tar failed while building the fixture");

    let contents = fs::read(&artifact).unwrap();
    let checksum = if valid_checksum {
        sha256(&contents)
    } else {
        sha256(b"not the artifact")
    };
    fs::write(
        root.join("SHA256SUMS"),
        format!("{checksum}  {artifact_name}\n"),
    )
    .unwrap();
    artifact
}

fn run_installer(root: &TempDir, dist: &Path, prefix: &Path) -> Output {
    let home = root.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh");
    Command::new("sh")
        .arg(script)
        .env("LH_VERSION", VERSION)
        .env("LH_INSTALL_SOURCE_DIR", dist)
        .env("LH_INSTALL_DIR", prefix)
        .env("HOME", &home)
        .env("XDG_DATA_HOME", root.path().join("xdg"))
        .env_remove("LH_TARGET")
        .env_remove("LH_NO_EXTRAS")
        .env_remove("LH_INSTALL_BASE_URL")
        .output()
        .expect("sh must be available to run install.sh")
}

#[test]
fn installer_installs_a_verified_binary_and_extras() {
    let root = TempDir::new().unwrap();
    let dist = root.path().join("dist");
    make_dist(&dist, VERSION, true);
    let prefix = root.path().join("bin");

    let output = run_installer(&root, &dist, &prefix);
    assert!(
        output.status.success(),
        "installer failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let installed = prefix.join("lh");
    assert!(
        installed.is_file(),
        "installer did not write {}",
        installed.display()
    );
    let mode = fs::metadata(&installed).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755, "installed binary is not executable");

    let version = Command::new(&installed).arg("--version").output().unwrap();
    assert!(version.status.success());
    assert!(
        String::from_utf8_lossy(&version.stdout).contains(VERSION),
        "installed binary did not run"
    );

    let data = root.path().join("xdg");
    assert!(data.join("bash-completion/completions/lh").is_file());
    assert!(data.join("zsh/site-functions/_lh").is_file());
    assert!(data.join("fish/vendor_completions.d/lh.fish").is_file());
    assert!(data.join("man/man1/lh.1").is_file());
}

#[test]
fn installer_refuses_a_checksum_mismatch_and_installs_nothing() {
    let root = TempDir::new().unwrap();
    let dist = root.path().join("dist");
    make_dist(&dist, VERSION, false);
    let prefix = root.path().join("bin");
    fs::create_dir_all(&prefix).unwrap();
    let untouched = prefix.join("lh");
    fs::write(&untouched, "previous install\n").unwrap();

    let output = run_installer(&root, &dist, &prefix);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "installer accepted a bad checksum"
    );
    assert!(
        stderr.contains("checksum mismatch"),
        "error did not name the mismatch:\n{stderr}"
    );
    assert_eq!(
        fs::read_to_string(&untouched).unwrap(),
        "previous install\n",
        "a failed verification replaced the existing binary"
    );
}
