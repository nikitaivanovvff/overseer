//! The daemon process: owns `AgentRegistry` + `SessionManager` + the IPC
//! socket across TUI restarts (AGENTS.md "Daemon split"). `overseer daemon`
//! runs this directly; the TUI auto-spawns one detached if the socket isn't
//! reachable, then attaches to it as a client.
//!
//! One daemon per user at a stable path — every repo's agents live under the
//! same daemon, same as a single tmux server backs every session.

use std::fs::{self, File};
use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::agent::AgentRegistry;
use crate::config::Config;
use crate::git::GitClient;
use crate::ipc::{self, client, AppCtx};
use crate::session::SessionManager;

/// `$XDG_RUNTIME_DIR/overseer` if set (and non-empty), else `/tmp/overseer-$UID`
/// — a stable, per-user location so every repo's TUI finds the same daemon.
pub fn default_socket_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("overseer");
        }
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/overseer-{uid}"))
}

pub fn default_socket_path() -> PathBuf {
    default_socket_dir().join("daemon.sock")
}

fn ensure_socket_dir(socket: &Path) -> Result<()> {
    if let Some(dir) = socket.parent() {
        fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to set permissions on {}", dir.display()))?;
    }
    Ok(())
}

fn lockfile_path(socket: &Path) -> PathBuf {
    socket.with_file_name("daemon.pid")
}

/// An exclusive `flock` on the lockfile next to `socket`, held for the life of
/// the daemon process. A second daemon targeting the same socket fails to
/// acquire it immediately (`LOCK_NB`) rather than racing the first for the
/// socket file. The OS releases the lock the instant this process dies (crash
/// included), so a stale lockfile left on disk is never mistaken for a live
/// daemon — only a held lock counts.
struct DaemonLock(#[allow(dead_code)] File);

impl DaemonLock {
    fn acquire(socket: &Path) -> Result<Self> {
        let path = lockfile_path(socket);
        let file = File::create(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        anyhow::ensure!(
            ret == 0,
            "another overseer daemon already holds the lock at {}",
            path.display()
        );
        let mut pid_file = &file;
        let _ = write!(pid_file, "{}", std::process::id());
        Ok(Self(file))
    }
}

/// Runs the daemon: binds the socket, serves requests, watches session exits.
/// Blocks until the process is killed (there is no graceful in-process stop
/// short of `overseer shutdown`, which recursively drops every agent and lets
/// the process exit on its own).
pub fn run_daemon(socket: PathBuf) -> Result<()> {
    ensure_socket_dir(&socket)?;
    let _lock = DaemonLock::acquire(&socket).context(
        "failed to become the daemon -- is another `overseer daemon` already running on this socket?",
    )?;

    let ctx = Arc::new(AppCtx {
        registry: Arc::new(AgentRegistry::new()),
        sessions: Arc::new(SessionManager::new()),
        socket: socket.clone(),
        git: Arc::new(GitClient::new()),
        config: Arc::new(Config::load()),
        watch_sessions: true,
        shutdown_notify: Arc::new(tokio::sync::Notify::new()),
    });

    ipc::serve_blocking(ctx, socket, None)?;
    Ok(())
}

/// Ensures a daemon is reachable at `socket`, spawning one (detached from the
/// caller's controlling terminal, tmux-server style) if a quick connect probe
/// fails. Retries the probe with backoff until the freshly spawned daemon
/// comes up or the attempt budget runs out.
///
/// Not yet called outside tests — the TUI still runs its own in-process
/// registry/session/socket (pre-daemon-split). Wired into `tui::run_tui`'s
/// real-mode startup once the attach client lands.
#[allow(dead_code)]
pub fn ensure_daemon_running(socket: &Path) -> Result<()> {
    if client::connect(socket).is_ok() {
        return Ok(());
    }

    spawn_detached(socket)?;

    let mut delay = Duration::from_millis(50);
    for _ in 0..20 {
        std::thread::sleep(delay);
        if client::connect(socket).is_ok() {
            return Ok(());
        }
        delay = (delay * 2).min(Duration::from_millis(500));
    }
    anyhow::bail!("daemon at {} did not become reachable in time", socket.display())
}

/// Spawns `overseer daemon --socket <socket>` detached from this process's
/// controlling terminal (`setsid`), so it outlives the TUI and the terminal
/// session that launched it — the same guarantee AGENTS.md already promises
/// for agent PTYs, now extended to the daemon itself. Stdio goes to a log
/// file next to the socket rather than being inherited.
fn spawn_detached(socket: &Path) -> Result<()> {
    ensure_socket_dir(socket)?;
    let exe = std::env::current_exe().context("failed to resolve overseer's own binary path")?;
    let log_path = socket.with_file_name("daemon.log");
    let log_out = File::create(&log_path)
        .with_context(|| format!("failed to create {}", log_path.display()))?;
    let log_err = log_out.try_clone().context("failed to clone daemon log handle")?;

    let mut cmd = Command::new(exe);
    cmd.arg("daemon")
        .arg("--socket")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(log_out)
        .stderr(log_err);
    // Safety: the closure only calls async-signal-safe libc functions
    // (setsid), as required between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().context("failed to spawn `overseer daemon`")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::EnvGuard;

    #[test]
    fn default_socket_dir_prefers_xdg_runtime_dir() {
        let _env = EnvGuard::set("XDG_RUNTIME_DIR", "/run/user/1000");
        assert_eq!(default_socket_dir(), PathBuf::from("/run/user/1000/overseer"));
    }

    #[test]
    fn default_socket_dir_falls_back_to_tmp_with_uid() {
        let _env = EnvGuard::unset("XDG_RUNTIME_DIR");
        let dir = default_socket_dir();
        let uid = unsafe { libc::getuid() };
        assert_eq!(dir, PathBuf::from(format!("/tmp/overseer-{uid}")));
    }

    #[test]
    fn default_socket_dir_ignores_empty_xdg_runtime_dir() {
        let _env = EnvGuard::set("XDG_RUNTIME_DIR", "");
        let dir = default_socket_dir();
        // `Path::starts_with` matches whole components, not string prefixes —
        // "overseer-" alone is never a full component of "overseer-501".
        assert!(dir.to_string_lossy().starts_with("/tmp/overseer-"));
    }

    #[test]
    fn default_socket_path_is_dir_joined_with_daemon_sock() {
        let _env = EnvGuard::set("XDG_RUNTIME_DIR", "/run/user/1000");
        assert_eq!(default_socket_path(), PathBuf::from("/run/user/1000/overseer/daemon.sock"));
    }

    /// macOS SUN_LEN limit is 104 bytes — keep the path short (see
    /// `ipc/mod.rs`'s test helper for the same constraint).
    fn unique_test_socket(name: &str) -> PathBuf {
        let id = &uuid::Uuid::new_v4().to_string()[..8];
        PathBuf::from(format!("/tmp/ovsr-d-{name}-{id}")).join("d.sock")
    }

    #[test]
    fn lockfile_prevents_a_second_daemon_on_the_same_socket() {
        let socket = unique_test_socket("lock");
        ensure_socket_dir(&socket).unwrap();

        let first = DaemonLock::acquire(&socket).expect("first lock should succeed");
        let second = DaemonLock::acquire(&socket);
        assert!(second.is_err(), "a second lock on the same socket must fail");

        drop(first);
        let third = DaemonLock::acquire(&socket);
        assert!(third.is_ok(), "lock must be released when the holder drops");

        let _ = std::fs::remove_dir_all(socket.parent().unwrap());
    }

    #[test]
    fn ensure_socket_dir_creates_it_with_owner_only_permissions() {
        let socket = unique_test_socket("perm");
        ensure_socket_dir(&socket).unwrap();
        let dir = socket.parent().unwrap();
        let mode = fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ensure_daemon_running_is_a_noop_when_already_reachable() {
        let socket = unique_test_socket("reachable");
        ensure_socket_dir(&socket).unwrap();
        let ctx = Arc::new(AppCtx {
            registry: Arc::new(AgentRegistry::new()),
            sessions: Arc::new(SessionManager::dry_run()),
            socket: socket.clone(),
            git: Arc::new(GitClient::dry_run()),
            config: Arc::new(Config::default()),
            watch_sessions: false,
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        });
        let socket_clone = socket.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            if let Err(e) = ipc::serve_blocking(ctx, socket_clone, Some(ready_tx)) {
                eprintln!("test server error: {e}");
            }
        });
        ready_rx.recv().expect("server failed to start");

        // Must not attempt to spawn anything — a spawn attempt against a
        // fake, unwritable exe path would surface as an error here.
        ensure_daemon_running(&socket).expect("already-reachable socket must be a no-op success");

        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_dir_all(socket.parent().unwrap());
    }
}
