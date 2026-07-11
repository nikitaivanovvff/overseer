//! Integration test for FIX B (`kill.rs`'s process-table-scan pid
//! discovery, DAEMON-LOCK-SAFETY — `crates/overseer-core/src/kill.rs`):
//! spawns a *real* `overseer daemon` process, then reproduces the observed
//! state of the 2026-07-11 incident — the daemon alive and holding the
//! `flock`, but with an unreadable `daemon.pid` and no reachable socket —
//! and asserts `overseer kill` recovers it anyway (finds the daemon by
//! scanning the process table for `daemon --socket <this socket>` in its
//! argv, force-kills it, and cleans up the stale files) rather than bailing
//! out wedged forever the way it did pre-fix.
//!
//! Lives under this bin crate's `tests/`, not `kill.rs`'s own `#[cfg(test)]`
//! module, specifically to get Cargo's `CARGO_BIN_EXE_overseer` env var:
//! that's only set for targets that *depend on* the `overseer` binary
//! (integration tests, benches, examples), never in `overseer-core`'s own
//! test harness — and driving the documented CLI surface (`overseer kill
//! --socket <path>`) is exactly what a user hitting this incident would run
//! by hand anyway.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn overseer_bin() -> &'static str {
    env!("CARGO_BIN_EXE_overseer")
}

/// macOS SUN_LEN limit is 104 bytes — keep the path short (mirrors
/// `src/daemon.rs`/`src/kill.rs`'s own `unique_test_socket`/`unique_socket`
/// helpers).
fn unique_socket(name: &str) -> PathBuf {
    let id = &uuid::Uuid::new_v4().to_string()[..8];
    PathBuf::from(format!("/tmp/ovsr-ki-{name}-{id}")).join("d.sock")
}

fn pid_is_alive(pid: i32) -> bool {
    let ret = unsafe { libc::kill(pid, 0) };
    // EPERM still means "alive, just not ours to signal again" — same
    // reasoning as `kill.rs`'s own `pid_is_alive`.
    ret == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

/// Owns the spawned daemon child and its socket directory; guarantees both
/// are gone — no stray process, no stray files — even if an assertion below
/// panics mid-test.
struct DaemonGuard {
    child: Child,
    dir: PathBuf,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        // Best-effort: by the time this runs the daemon may already be
        // dead (that's the whole point of the test) — ignore errors from a
        // signal or wait against an already-reaped/nonexistent process.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn kill_recovers_a_daemon_via_ps_scan_when_its_pid_and_socket_are_both_gone() {
    let socket = unique_socket("recover");
    let dir = socket.parent().unwrap().to_path_buf();

    let child = Command::new(overseer_bin())
        .arg("daemon")
        .arg("--socket")
        .arg(&socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn a real `overseer daemon`");
    let mut guard = DaemonGuard { child, dir };
    let daemon_pid = guard.child.id() as i32;

    let socket_for_probe = socket.clone();
    let bound = wait_until(|| UnixStream::connect(&socket_for_probe).is_ok(), Duration::from_secs(5));
    assert!(bound, "daemon at {} never became reachable", socket.display());

    // Reproduce the 2026-07-11 incident's observed state: `daemon.pid`
    // truncated/unreadable and the socket file gone, while the daemon
    // itself is still alive and holding the flock (BUG A used to cause
    // exactly this via a losing daemon's own truncating `File::create`;
    // this test reproduces the *effect* directly, per DAEMON-LOCK-SAFETY's
    // non-goal of not chasing the incident's actual, undetermined deleter).
    let lockfile = lockfile_path(&socket);
    std::fs::write(&lockfile, "").expect("failed to truncate the daemon's own lockfile");
    std::fs::remove_file(&socket).expect("failed to delete the daemon's own socket");

    let output = Command::new(overseer_bin())
        .arg("kill")
        .arg("--socket")
        .arg(&socket)
        .output()
        .expect("failed to run `overseer kill`");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "`overseer kill` must succeed: stdout={stdout} stderr={stderr}");
    assert!(
        stdout.contains("force-killed"),
        "expected the forced-kill path (ps-scan pid discovery), got stdout: {stdout}"
    );

    // We're the daemon's real parent (unlike a normal `overseer kill`, run
    // from a separate process from the daemon it targets) — reap it so
    // `pid_is_alive` reflects reality rather than an un-reaped zombie.
    let _ = guard.child.wait();

    assert!(!pid_is_alive(daemon_pid), "the daemon process (pid {daemon_pid}) must actually be dead");
    assert!(!lockfile.exists(), "the lockfile must be cleaned up: {}", lockfile.display());
    assert!(!socket.exists(), "the socket must stay cleaned up: {}", socket.display());
}

fn lockfile_path(socket: &Path) -> PathBuf {
    socket.with_file_name("daemon.pid")
}
