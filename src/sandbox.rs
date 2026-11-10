//! OS-level sandbox for `bash` and `bg`.
//!
//! # Backends
//!
//! The primary backend is bubblewrap (`bwrap`). A wrapped command runs in fresh
//! user, mount, PID, IPC, UTS, cgroup, and network namespaces with a read-only
//! root, a private `/tmp`, an empty `/home`, no capabilities, a cleared
//! environment, and a classic-BPF seccomp filter that rejects namespace,
//! mount, module, keyring, tracing, and BPF syscalls.
//!
//! When bubblewrap is not installed the harness falls back to Landlock. That
//! ruleset restricts filesystem rights and (with Landlock ABI 4 or newer) TCP
//! bind/connect under `no_new_privs`. Landlock is additive, so the fallback
//! cannot hide `$HOME`, cannot make `.git` read-only, and does not cover UDP;
//! bubblewrap is stronger and is preferred whenever it exists.
//!
//! Both backends fail closed: [`Sandbox::new`] and [`Sandbox::wrap`] return an
//! actionable error instead of falling back to an unsandboxed run.

use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

#[cfg(target_os = "linux")]
use nix::libc;

use crate::config::SandboxMode;

/// Landlock ABI that can deny TCP network access. Older ABIs cannot close the
/// network, so the fallback refuses to run rather than leave it open.
const MIN_LANDLOCK_ABI: i32 = 4;

/// Fixed descriptor bwrap reads the seccomp program from. `dup2` in the child
/// clears close-on-exec, so the filter survives the exec of `bwrap`.
#[cfg(target_os = "linux")]
const SECCOMP_FD: libc::c_int = 200;

/// `PATH` used when the host does not export one. The sandbox always sets a
/// `PATH` so `bash -lc` can find commands.
const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxBackend {
    Bubblewrap,
    Landlock,
}

/// A configured sandbox for one workspace.
///
/// Construction checks the chosen backend once; every `wrap` call then builds
/// a fresh command. `Sandbox::new` is cheap enough to call at startup, and the
/// parent can share one instance with all tools through `Arc`.
#[derive(Debug, Clone)]
pub struct Sandbox {
    mode: SandboxMode,
    workspace: PathBuf,
    bwrap: Option<PathBuf>,
    backend: SandboxBackend,
}

impl Sandbox {
    /// Build a sandbox for `mode` and `workspace`.
    ///
    /// Fails closed: when a sandbox is requested but no usable backend exists,
    /// the error names the missing prerequisite and the `--no-sandbox` escape.
    /// `mode == Off` needs no backend and never fails.
    pub fn new(mode: SandboxMode, workspace: PathBuf) -> Result<Self, String> {
        if mode == SandboxMode::Off {
            return Ok(Self {
                mode,
                workspace,
                bwrap: None,
                backend: SandboxBackend::Landlock,
            });
        }
        let workspace = resolve_workspace(mode, &workspace)?;
        let (backend, bwrap) = detect_backend()?;
        Ok(Self {
            mode,
            workspace,
            bwrap,
            backend,
        })
    }

    pub fn mode(&self) -> SandboxMode {
        self.mode
    }

    pub fn backend(&self) -> SandboxBackend {
        self.backend
    }

    /// Name shown in diagnostics: `bubblewrap`, `landlock`, or `off`.
    pub fn label(&self) -> &'static str {
        match self.mode {
            SandboxMode::Off => "off",
            SandboxMode::ReadOnly | SandboxMode::WorkspaceWrite => match self.backend {
                SandboxBackend::Bubblewrap => "bubblewrap",
                SandboxBackend::Landlock => "landlock",
            },
        }
    }

    /// Wrap `command` so it runs inside the sandbox with `cwd` as its working
    /// directory. The returned command owns a process group, like the plain
    /// shell command it replaces.
    pub fn wrap(&self, command: &str, cwd: &Path) -> Result<tokio::process::Command, String> {
        match self.mode {
            SandboxMode::Off => Ok(plain_command(command, cwd)),
            SandboxMode::ReadOnly | SandboxMode::WorkspaceWrite => match self.backend {
                SandboxBackend::Bubblewrap => self.wrap_bwrap(command, cwd),
                SandboxBackend::Landlock => self.wrap_landlock(command, cwd),
            },
        }
    }

    fn wrap_bwrap(&self, command: &str, cwd: &Path) -> Result<tokio::process::Command, String> {
        let bwrap = self.bwrap.as_deref().ok_or_else(|| {
            "the bubblewrap backend is selected but no bwrap executable was recorded".to_string()
        })?;
        let cwd = self.validated_cwd(cwd)?;

        let mut args: Vec<OsString> = [
            "--die-with-parent",
            "--new-session",
            "--unshare-all",
            "--cap-drop",
            "ALL",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/home",
            "--tmpfs",
            "/root",
        ]
        .iter()
        .map(OsString::from)
        .collect();

        // Restore the workspace after the tmpfs mounts so a workspace under
        // /home or /root stays visible.
        let bind = if self.mode == SandboxMode::WorkspaceWrite {
            "--bind"
        } else {
            "--ro-bind"
        };
        push_bind(&mut args, bind, &self.workspace);

        // `.git` carries the repository's trust boundary; keep it read-only
        // even when the surrounding workspace is writable.
        if self.mode == SandboxMode::WorkspaceWrite {
            let git = self.workspace.join(".git");
            if git.exists() {
                push_bind(&mut args, "--ro-bind", &git);
            }
        }

        push_env(&mut args);
        args.push("--chdir".into());
        args.push(cwd.as_os_str().to_os_string());

        #[cfg(target_os = "linux")]
        let seccomp = seccomp_filter()?;
        #[cfg(target_os = "linux")]
        {
            args.push("--seccomp".into());
            args.push(SECCOMP_FD.to_string().into());
        }

        let mut cmd = tokio::process::Command::new(bwrap);
        cmd.args(&args)
            .arg("--")
            .arg("bash")
            .arg("-lc")
            .arg(command)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.as_std_mut().process_group(0);

        // `--seccomp` needs an fd number that survives the exec of bwrap.
        // Duplicate the filter onto a fixed descriptor in the child; `dup2`
        // clears close-on-exec. The parent's copy closes after spawn.
        #[cfg(target_os = "linux")]
        unsafe {
            cmd.as_std_mut().pre_exec(move || {
                if libc::dup2(seccomp.as_raw_fd(), SECCOMP_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // `dup2` is a no-op when the source already is SECCOMP_FD and
                // would leave close-on-exec set; clear it explicitly.
                if libc::fcntl(SECCOMP_FD, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        Ok(cmd)
    }

    fn wrap_landlock(&self, command: &str, cwd: &Path) -> Result<tokio::process::Command, String> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (command, cwd);
            Err(
                "the Landlock backend is only available on Linux; install bubblewrap (bwrap) or rerun with --no-sandbox to run unsandboxed"
                    .to_string(),
            )
        }

        #[cfg(target_os = "linux")]
        {
            // `libc` does not expose prctl on every target, so use the syscall.
            const PR_SET_NO_NEW_PRIVS: libc::c_long = 38;
            let cwd = self.validated_cwd(cwd)?;
            let temp = std::fs::canonicalize(std::env::temp_dir())
                .unwrap_or_else(|_| PathBuf::from("/tmp"));
            if temp == Path::new("/") {
                return Err(
                    "TMPDIR is the filesystem root; refusing to grant write access to the whole filesystem. Set TMPDIR to a real temp directory or rerun with --no-sandbox to run unsandboxed"
                        .to_string(),
                );
            }
            let ruleset = landlock_ruleset(self.mode, &self.workspace, &temp)?;
            let mut cmd = plain_command(command, &cwd);
            // Applied after fork and before exec. Landlock is inherited by
            // every descendant of this shell.
            unsafe {
                cmd.as_std_mut().pre_exec(move || {
                    if libc::syscall(
                        libc::SYS_prctl,
                        PR_SET_NO_NEW_PRIVS,
                        1 as libc::c_long,
                        0 as libc::c_long,
                        0 as libc::c_long,
                        0 as libc::c_long,
                    ) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::syscall(
                        libc::SYS_landlock_restrict_self,
                        ruleset.as_raw_fd() as libc::c_uint,
                        0u32,
                    ) < 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            Ok(cmd)
        }
    }

    /// Resolve `cwd` and refuse directories outside the workspace. A missing
    /// directory is left to the spawn path, which already reports it clearly.
    fn validated_cwd(&self, cwd: &Path) -> Result<PathBuf, String> {
        let resolved = match std::fs::canonicalize(cwd) {
            Ok(resolved) => resolved,
            Err(_) => return Ok(cwd.to_path_buf()),
        };
        if !resolved.starts_with(&self.workspace) {
            return Err(format!(
                "sandbox cwd {} is outside the workspace {}; use a cwd inside the workspace",
                resolved.display(),
                self.workspace.display()
            ));
        }
        Ok(resolved)
    }
}

/// Report which backend `mode` would use without constructing a sandbox.
///
/// With `Off` the answer is informational (what `--sandbox` would pick) and
/// only fails when neither backend is installed.
pub fn detect(mode: SandboxMode, workspace: &Path) -> Result<SandboxBackend, String> {
    if mode == SandboxMode::Off {
        if bwrap_path().is_some() {
            return Ok(SandboxBackend::Bubblewrap);
        }
        if landlock_abi().is_some_and(|abi| abi >= MIN_LANDLOCK_ABI) {
            return Ok(SandboxBackend::Landlock);
        }
        return Err(no_backend_error());
    }
    let _ = resolve_workspace(mode, workspace)?;
    detect_backend().map(|(backend, _)| backend)
}

fn no_backend_error() -> String {
    "sandbox requested but no backend is available: install bubblewrap (bwrap) or enable Landlock; rerun with --no-sandbox to run unsandboxed".to_string()
}

fn detect_backend() -> Result<(SandboxBackend, Option<PathBuf>), String> {
    if let Some(path) = bwrap_path() {
        return Ok((SandboxBackend::Bubblewrap, Some(path)));
    }
    match landlock_abi() {
        Some(abi) if abi >= MIN_LANDLOCK_ABI => Ok((SandboxBackend::Landlock, None)),
        Some(abi) => Err(format!(
            "sandbox requested but no backend is available: bubblewrap (bwrap) is missing and Landlock ABI {abi} cannot restrict TCP network access (ABI {MIN_LANDLOCK_ABI} or newer is required); install bubblewrap (bwrap) or rerun with --no-sandbox to run unsandboxed"
        )),
        None => Err(no_backend_error()),
    }
}

fn resolve_workspace(mode: SandboxMode, workspace: &Path) -> Result<PathBuf, String> {
    let resolved = std::fs::canonicalize(workspace).map_err(|err| {
        format!(
            "sandbox workspace {} is not accessible: {err}; pass an existing project directory",
            workspace.display()
        )
    })?;
    if !resolved.is_dir() {
        return Err(format!(
            "sandbox workspace {} is not a directory",
            resolved.display()
        ));
    }
    if mode == SandboxMode::WorkspaceWrite && resolved == Path::new("/") {
        return Err(
            "sandbox workspace is the filesystem root; workspace-write would expose the whole filesystem. Run from a project directory or rerun with --no-sandbox to run unsandboxed"
                .to_string(),
        );
    }
    Ok(resolved)
}

/// Locate the bubblewrap executable. `LH_BWRAP` overrides the name/path; tests
/// point it at a missing file to exercise the fail-closed path.
fn bwrap_path() -> Option<PathBuf> {
    let name = std::env::var_os("LH_BWRAP").unwrap_or_else(|| OsString::from("bwrap"));
    let path = PathBuf::from(name.clone());
    if path.components().count() > 1 {
        return is_executable(&path).then_some(path);
    }
    let search = std::env::var_os("PATH")?;
    std::env::split_paths(&search)
        .map(|dir| dir.join(&name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn push_bind(args: &mut Vec<OsString>, flag: &str, path: &Path) {
    args.push(flag.into());
    args.push(path.as_os_str().to_os_string());
    args.push(path.as_os_str().to_os_string());
}

/// Clear the environment, then restore only what a shell needs. Secrets,
/// agent sockets, and desktop session variables are deliberately dropped.
fn push_env(args: &mut Vec<OsString>) {
    args.push("--clearenv".into());
    let mut saw_path = false;
    for name in [
        "PATH", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "TZ", "USER", "LOGNAME", "SHELL",
    ] {
        if let Some(value) = std::env::var_os(name)
            && !value.is_empty()
        {
            if name == "PATH" {
                saw_path = true;
            }
            args.push("--setenv".into());
            args.push(name.into());
            args.push(value);
        }
    }
    if !saw_path {
        args.push("--setenv".into());
        args.push("PATH".into());
        args.push(DEFAULT_PATH.into());
    }
    // `$HOME` is a real directory on the host; point it at a path that exists
    // nowhere so `~/.ssh`, `~/.gitconfig`, and friends cannot be read, and
    // writes to `$HOME` fail.
    args.push("--setenv".into());
    args.push("HOME".into());
    args.push("/nonexistent".into());
    args.push("--setenv".into());
    args.push("TMPDIR".into());
    args.push("/tmp".into());
}

/// The plain shell command used when no sandbox is enabled. Mirrors
/// `crate::tools::bash::shell_command` so both paths agree on shell semantics
/// and process-group ownership.
fn plain_command(command: &str, cwd: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-lc")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.as_std_mut().process_group(0);
    cmd
}

/// Query the Landlock ABI. `None` means the kernel does not implement it.
#[cfg(target_os = "linux")]
fn landlock_abi() -> Option<i32> {
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    // SAFETY: the kernel only reads the size and version arguments when the
    // ruleset pointer is null.
    let version = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    (version >= 1).then_some(version as i32)
}

#[cfg(not(target_os = "linux"))]
fn landlock_abi() -> Option<i32> {
    None
}

#[cfg(target_os = "linux")]
mod landlock {
    use std::ffi::CString;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use nix::libc;

    use crate::config::SandboxMode;

    const RULE_PATH_BENEATH: libc::c_uint = 1;

    const FS_EXECUTE: u64 = 1 << 0;
    const FS_WRITE_FILE: u64 = 1 << 1;
    const FS_READ_FILE: u64 = 1 << 2;
    const FS_READ_DIR: u64 = 1 << 3;
    const FS_REMOVE_DIR: u64 = 1 << 4;
    const FS_REMOVE_FILE: u64 = 1 << 5;
    const FS_MAKE_CHAR: u64 = 1 << 6;
    const FS_MAKE_DIR: u64 = 1 << 7;
    const FS_MAKE_REG: u64 = 1 << 8;
    const FS_MAKE_SOCK: u64 = 1 << 9;
    const FS_MAKE_FIFO: u64 = 1 << 10;
    const FS_MAKE_BLOCK: u64 = 1 << 11;
    const FS_MAKE_SYM: u64 = 1 << 12;
    const FS_REFER: u64 = 1 << 13;
    const FS_TRUNCATE: u64 = 1 << 14;

    const NET_BIND_TCP: u64 = 1 << 0;
    const NET_CONNECT_TCP: u64 = 1 << 1;

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
    }

    /// Kernel UAPI marks this packed, so it is exactly 12 bytes. The
    /// `landlock_add_rule` argument after the pointer is flags, not a size.
    #[repr(C, packed)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: RawFd,
    }

    pub(super) fn ruleset(
        mode: SandboxMode,
        workspace: &Path,
        temp: &Path,
    ) -> Result<OwnedFd, String> {
        let abi = super::landlock_abi().ok_or_else(|| {
            "sandbox requested but Landlock is not available on this kernel; install bubblewrap (bwrap) or rerun with --no-sandbox to run unsandboxed"
                .to_string()
        })?;
        if abi < super::MIN_LANDLOCK_ABI {
            return Err(format!(
                "sandbox requested but bubblewrap (bwrap) is missing and Landlock ABI {abi} cannot restrict TCP network access; install bubblewrap (bwrap) or rerun with --no-sandbox to run unsandboxed"
            ));
        }

        let read = FS_EXECUTE | FS_READ_FILE | FS_READ_DIR;
        let write = write_rights(abi);
        let attr = RulesetAttr {
            handled_access_fs: read | write,
            handled_access_net: NET_BIND_TCP | NET_CONNECT_TCP,
        };
        // SAFETY: the kernel reads exactly `size_of::<RulesetAttr>()` bytes
        // from a valid, correctly typed pointer.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                std::mem::size_of::<RulesetAttr>() as libc::size_t,
                0u32,
            )
        };
        if fd < 0 {
            return Err(format!(
                "could not create a Landlock ruleset: {}",
                io::Error::last_os_error()
            ));
        }
        // SAFETY: a non-negative return value is a new owned file descriptor.
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };

        add_rule(ruleset.as_raw_fd(), Path::new("/"), read)?;
        if mode == SandboxMode::WorkspaceWrite {
            add_rule(ruleset.as_raw_fd(), workspace, read | write)?;
        }
        // Programs need a writable temp directory in both modes. Unlike
        // bubblewrap this is the host `/tmp`, not a private tmpfs.
        add_rule(ruleset.as_raw_fd(), temp, read | write)?;
        Ok(ruleset)
    }

    fn write_rights(abi: i32) -> u64 {
        let mut rights = FS_WRITE_FILE
            | FS_REMOVE_DIR
            | FS_REMOVE_FILE
            | FS_MAKE_CHAR
            | FS_MAKE_DIR
            | FS_MAKE_REG
            | FS_MAKE_SOCK
            | FS_MAKE_FIFO
            | FS_MAKE_BLOCK
            | FS_MAKE_SYM;
        // Newer rights must not be requested on kernels that lack them.
        if abi >= 2 {
            rights |= FS_REFER;
        }
        if abi >= 3 {
            rights |= FS_TRUNCATE;
        }
        rights
    }

    fn add_rule(ruleset: RawFd, path: &Path, access: u64) -> Result<(), String> {
        let raw = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| format!("sandbox path {} contains a NUL byte", path.display()))?;
        // SAFETY: `raw` is a valid NUL-terminated path.
        let fd = unsafe { libc::open(raw.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(format!(
                "could not open sandbox path {}: {}",
                path.display(),
                io::Error::last_os_error()
            ));
        }
        // SAFETY: a non-negative return value is a new owned file descriptor.
        let dir = unsafe { OwnedFd::from_raw_fd(fd) };
        let attr = PathBeneathAttr {
            allowed_access: access,
            parent_fd: dir.as_raw_fd(),
        };
        // SAFETY: the kernel reads the fixed-size packed structure for this
        // rule type. The last argument is flags, not a size.
        let result = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset as libc::c_uint,
                RULE_PATH_BENEATH,
                &attr as *const PathBeneathAttr,
                0u32,
            )
        };
        if result < 0 {
            return Err(format!(
                "could not add a Landlock rule for {}: {}",
                path.display(),
                io::Error::last_os_error()
            ));
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn landlock_ruleset(
    mode: SandboxMode,
    workspace: &Path,
    temp: &Path,
) -> Result<std::os::fd::OwnedFd, String> {
    landlock::ruleset(mode, workspace, temp)
}

/// Syscall numbers blocked by the seccomp filter, in the order the filter
/// checks them: mount, umount2, pivot_root, chroot, kexec_load,
/// kexec_file_load, reboot, swapon, swapoff, init_module, finit_module,
/// delete_module, open_by_handle_at, name_to_handle_at, userfaultfd, bpf,
/// perf_event_open, keyctl, add_key, request_key, ptrace, process_vm_readv,
/// process_vm_writev, setns, unshare.
///
/// The numbers are spelled out so the filter does not depend on libc covering
/// every syscall constant on every architecture.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const BLOCKED_SYSCALLS: &[libc::c_long] = &[
    165, 166, 155, 161, 246, 320, 169, 167, 168, 175, 313, 176, 304, 303, 323, 321, 298, 250, 248,
    249, 101, 310, 311, 308, 272,
];

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const BLOCKED_SYSCALLS: &[libc::c_long] = &[
    40, 39, 41, 51, 104, 294, 142, 224, 225, 105, 273, 106, 265, 264, 282, 280, 241, 219, 217, 218,
    117, 270, 271, 268, 97,
];

/// Classic-BPF filter installed by bubblewrap: every syscall on the list
/// returns `EPERM`, everything else is allowed.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn seccomp_filter() -> Result<std::os::fd::OwnedFd, String> {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::fd::{FromRawFd, OwnedFd};

    const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
    const AUDIT_ARCH_AARCH64: u32 = 0xC000_00B7;
    const MFD_CLOEXEC: libc::c_uint = 0x0001;
    const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;

    // The audit arch depends on the userspace ABI, not the kernel arch.
    let arch = if cfg!(target_arch = "aarch64") {
        AUDIT_ARCH_AARCH64
    } else {
        AUDIT_ARCH_X86_64
    };

    let blocked = BLOCKED_SYSCALLS;

    let insn = |code: u32, jt: u8, jf: u8, k: u32| libc::sock_filter {
        code: code as u16,
        jt,
        jf,
        k,
    };
    let errno_eperm = 0x0005_0000 | (libc::EPERM as u32);

    let mut program = Vec::with_capacity(4 + blocked.len() * 2);
    program.push(insn(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 0, 0, 4)); // seccomp_data.arch
    // Matching arch skips the kill; any other ABI is not our filter's target.
    program.push(insn(
        libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
        1,
        0,
        arch,
    ));
    program.push(insn(
        libc::BPF_RET | libc::BPF_K,
        0,
        0,
        SECCOMP_RET_KILL_PROCESS,
    ));
    program.push(insn(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 0, 0, 0)); // seccomp_data.nr
    for &nr in blocked {
        program.push(insn(
            libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
            0,
            1,
            nr as u32,
        ));
        program.push(insn(libc::BPF_RET | libc::BPF_K, 0, 0, errno_eperm));
    }
    program.push(insn(libc::BPF_RET | libc::BPF_K, 0, 0, SECCOMP_RET_ALLOW));

    // SAFETY: `memfd_create` returns a new owned fd. The filter bytes are read
    // from a live `sock_filter` slice.
    let fd = unsafe { libc::memfd_create(c"lh-seccomp".as_ptr(), MFD_CLOEXEC) };
    if fd < 0 {
        return Err(format!(
            "could not create a seccomp filter: {}; rerun with --no-sandbox to run unsandboxed",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: a non-negative return value is a new owned file descriptor.
    let mut file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    let bytes = unsafe {
        std::slice::from_raw_parts(
            program.as_ptr().cast::<u8>(),
            program.len() * std::mem::size_of::<libc::sock_filter>(),
        )
    };
    file.write_all(bytes)
        .map_err(|err| format!("could not write the seccomp filter: {err}"))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|err| format!("could not rewind the seccomp filter: {err}"))?;
    Ok(OwnedFd::from(file))
}

/// Other Linux architectures have no audit-arch table here. Fail closed rather
/// than run bubblewrap without the syscall filter.
#[cfg(all(
    target_os = "linux",
    not(any(target_arch = "x86_64", target_arch = "aarch64"))
))]
fn seccomp_filter() -> Result<std::os::fd::OwnedFd, String> {
    Err(format!(
        "the seccomp filter is not defined for {}; rerun with --no-sandbox to run unsandboxed",
        std::env::consts::ARCH
    ))
}
