//! `tot serve`: a local HTTP API for scripts, cron jobs, CI, and small UIs.
//!
//! Endpoints (all under `/v1`, all require `Authorization: Bearer <token>`):
//!
//! - `GET  /health`
//! - `GET  /sessions`, `POST /sessions`, `GET /sessions/{id}`
//! - `POST /sessions/{id}/turns`
//! - `GET  /sessions/{id}/events` (Server-Sent Events)
//! - `POST /sessions/{id}/cancel`
//!
//! Security stance:
//! - Loopback only unless `--allow-non-loopback` is passed, which warns.
//! - A fresh random token per start, written to a `0600` file whose path is
//!   printed; the token itself is never printed, logged, or echoed.
//! - The token is checked before routing, so an unauthenticated caller
//!   cannot tell a missing session from a bad token.
//! - No CORS headers; a request that carries `Origin` is refused, so a page
//!   in a browser cannot drive the server.
//! - Sessions use the same session store, path jail, scope gate, sandbox,
//!   and config as ACP and the TUI ([`crate::host`]). An HTTP session cannot
//!   answer a permission prompt, so calls that would ask are denied unless
//!   the session was created with a mode that runs them (see [`api`]).
//! - No telemetry: nothing leaves the machine except model requests to the
//!   configured endpoint.

mod api;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::host::Host;
use crate::paths::Paths;

pub use api::PermissionMode;

/// Options from `tot serve`.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub bind: String,
    pub allow_non_loopback: bool,
    pub token_file: Option<PathBuf>,
    /// Let a session request `permission_mode: "yolo"`.
    pub allow_yolo: bool,
    pub engagement: Option<String>,
    pub no_session: bool,
    pub no_redact: bool,
}

/// How long shutdown waits for running turns to write their cancellation.
const SHUTDOWN_TURN_WAIT: Duration = Duration::from_secs(10);

/// Parse `--bind`. `localhost` means `127.0.0.1`; host names are refused so
/// the loopback check cannot be fooled by DNS.
pub fn parse_bind(bind: &str) -> Result<SocketAddr> {
    if let Some(port) = bind.strip_prefix("localhost:") {
        let port: u16 = port
            .parse()
            .with_context(|| format!("invalid port in --bind {bind:?}"))?;
        return Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
    }
    bind.parse().with_context(|| {
        format!("--bind {bind:?} must be IP:PORT, for example 127.0.0.1:0 or [::1]:8080")
    })
}

/// Refuse a non-loopback address unless the flag was passed. Returns the
/// warning to print when it was.
pub fn check_bind(address: SocketAddr, allow_non_loopback: bool) -> Result<Option<String>> {
    if address.ip().is_loopback() {
        return Ok(None);
    }
    if !allow_non_loopback {
        bail!(
            "refusing to bind {address}: it is not a loopback address. `tot serve` exposes a session that can edit files and run commands; bind 127.0.0.1, or pass --allow-non-loopback if you understand that anyone who can reach this address and read the token can drive the harness"
        );
    }
    Ok(Some(format!(
        "warning: tot serve is listening on non-loopback address {address}. Anyone who can reach it and obtain the token can drive the harness; the traffic is plain HTTP, so the token crosses the network unencrypted. Prefer 127.0.0.1 and an SSH tunnel."
    )))
}

/// A fresh 256-bit token, hex encoded.
fn generate_token() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .context("cannot read /dev/urandom for the server token")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Write the token to `path`, created with mode 0600. A stale regular file
/// is replaced; a symlink or anything else at the path is refused.
fn write_token_file(path: &Path, token: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => {
            std::fs::remove_file(path)
                .with_context(|| format!("cannot replace {}", path.display()))?;
        }
        Ok(_) => bail!(
            "{} exists and is not a regular file; remove it or pass another --token-file",
            path.display()
        ),
        Err(_) => {}
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("cannot create token file {}", path.display()))?;
    file.write_all(token.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("cannot write token file {}", path.display()))?;
    Ok(())
}

/// Run the server until SIGINT or SIGTERM.
pub async fn run(config: Config, paths: Paths, options: ServeOptions) -> Result<i32> {
    let address = parse_bind(&options.bind)?;
    let warning = check_bind(address, options.allow_non_loopback)?;
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("cannot bind {address}"))?;
    let local = listener.local_addr()?;
    if let Some(warning) = &warning {
        eprintln!("{warning}");
        tracing::warn!(%local, "tot serve bound a non-loopback address");
    }

    let token = generate_token()?;
    let token_file = options.token_file.clone().unwrap_or_else(|| {
        paths
            .data_dir
            .join("serve")
            .join(format!("token-{}", local.port()))
    });
    write_token_file(&token_file, &token)?;

    let host = Host {
        config,
        process_paths: paths,
        engagement: options.engagement.clone(),
        no_session: options.no_session,
        no_redact: options.no_redact,
    };
    let shutdown = CancellationToken::new();
    let state = Arc::new(api::State::new(
        host,
        token,
        options.allow_yolo,
        shutdown.clone(),
    ));

    // One machine-readable line on stdout; humans read stderr.
    println!(
        "{}",
        json!({
            "listening": format!("http://{local}"),
            "token_file": token_file.display().to_string(),
        })
    );
    eprintln!(
        "tot serve listening on http://{local}\ntoken file: {}\nsend `Authorization: Bearer $(cat <token file>)`; Ctrl+C stops the server",
        token_file.display()
    );
    tracing::info!(%local, "tot serve started");

    let mut connections = JoinSet::new();
    let stop = stop_signal();
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            accepted = listener.accept() => {
                let (stream, _peer) = match accepted {
                    Ok(pair) => pair,
                    Err(error) => {
                        tracing::warn!(%error, "accept failed");
                        continue;
                    }
                };
                let state = state.clone();
                let shutdown = shutdown.clone();
                connections.spawn(async move {
                    let service = service_fn(move |request| {
                        let state = state.clone();
                        async move { Ok::<_, std::convert::Infallible>(api::handle(state, request).await) }
                    });
                    let connection = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service);
                    tokio::pin!(connection);
                    tokio::select! {
                        _ = connection.as_mut() => {}
                        _ = shutdown.cancelled() => {
                            connection.as_mut().graceful_shutdown();
                            let _ = tokio::time::timeout(Duration::from_secs(2), connection).await;
                        }
                    }
                });
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }

    eprintln!("tot serve stopping: cancelling running turns");
    drop(listener);
    // Refuse new work, cancel running turns, and wait until each has written
    // its cancellation (and published `done`); then end SSE streams and ask
    // every connection to close.
    state.shutdown(SHUTDOWN_TURN_WAIT).await;
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(3), async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    connections.abort_all();
    let _ = std::fs::remove_file(&token_file);
    eprintln!("tot serve stopped; sessions are resumable with `tot --resume <id>`");
    Ok(0)
}

async fn stop_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(term) => term,
        Err(error) => {
            tracing::warn!(%error, "cannot listen for SIGTERM");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_binds_need_no_flag() {
        for bind in ["127.0.0.1:0", "[::1]:8080", "localhost:9000", "127.0.0.2:1"] {
            let address = parse_bind(bind).unwrap();
            assert!(check_bind(address, false).unwrap().is_none(), "{bind}");
        }
    }

    #[test]
    fn non_loopback_binds_need_the_flag_and_warn() {
        for bind in ["0.0.0.0:0", "[::]:0", "192.168.1.5:8080"] {
            let address = parse_bind(bind).unwrap();
            assert!(check_bind(address, false).is_err(), "{bind}");
            let warning = check_bind(address, true).unwrap().unwrap();
            assert!(warning.starts_with("warning:"), "{warning}");
        }
        assert!(parse_bind("example.com:80").is_err());
    }

    #[test]
    fn the_token_file_is_private_and_replaces_only_regular_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("token");
        write_token_file(&path, "first").unwrap();
        write_token_file(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let parent = std::fs::metadata(path.parent().unwrap()).unwrap();
        assert_eq!(parent.permissions().mode() & 0o777, 0o700);

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(write_token_file(&link, "third").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
    }

    #[test]
    fn tokens_are_long_and_distinct() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }
}
