//! `overseer kill` — last-resort forceful cleanup for a daemon `overseer
//! shutdown` can't reach: wedged/deadlocked and never replying, or already
//! crashed with a stale socket/lockfile left behind (`AGENTS.md` "Cleanup").
//!
//! This is *not* a second way to gracefully end the daemon — `AGENTS.md`
//! "What to Avoid" forbids adding a second path that races
//! `Request::Shutdown`'s own notify-based stop signal, and this module
//! doesn't: it always tries that exact same graceful request first, bounded
//! by a short timeout, and only escalates to a raw `SIGKILL` once that
//! request has been given a real chance and failed to get a response. The
//! daemon is never asked to shut down twice by two different mechanisms
//! racing each other — either the graceful reply comes back in time (this
//! module's job ends there, no signal ever sent), or it doesn't and this
//! falls through to killing a daemon that, by then, has already proven
//! itself unresponsive.
//!
//! Kept as a distinct subcommand rather than folded into `overseer shutdown`
//! itself so existing scripts/callers of `shutdown` see no new
//! destructive-by-default behavior — `shutdown` stays exactly what it always
//! was: one graceful request, no timeout, no signal-kill fallback.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::daemon;
use crate::ipc::client;
use crate::ipc::protocol::Request;

/// How long the graceful `Request::Shutdown` attempt gets (including retries
/// across a possible daemon-still-starting-up race) before falling back to a
/// forceful kill. Long enough that a daemon under real load — recursively
/// dropping a large tree — still finishes normally and is never mistaken for
/// wedged; short enough that a genuinely unresponsive daemon doesn't leave
/// the caller waiting.
const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to poll for the daemon pid to actually disappear after
/// `SIGKILL`, before giving up waiting and cleaning up files anyway.
const DEATH_POLL_TIMEOUT: Duration = Duration::from_secs(2);
const DEATH_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// What `run` actually did, for the CLI to report back to the user.
#[derive(Debug)]
pub enum Outcome {
    /// Nothing was holding the daemon's lock — it was already dead. Any
    /// stale socket/lockfile left behind by the crash were cleaned up so a
    /// fresh daemon can bind cleanly.
    AlreadyDead { cleaned_socket: bool, cleaned_lockfile: bool },
    /// The graceful `Request::Shutdown` path answered within the timeout —
    /// no signal was ever sent.
    Graceful,
    /// The daemon didn't answer in time — force-killed directly, plus any
    /// orphaned agent PTY processes found as its direct children.
    Forced { daemon_pid: i32, reclaimed_children: usize },
}

/// The testable core: forceful cleanup against `socket`, using the real
/// timeouts. See `run_with_timeouts` for the parameterized version tests use
/// to keep the wedged-daemon and force-kill cases fast and deterministic.
pub fn run(socket: &Path) -> Result<Outcome> {
    run_with_timeouts(socket, GRACEFUL_TIMEOUT, DEATH_POLL_TIMEOUT, DEATH_POLL_INTERVAL)
}

fn run_with_timeouts(
    socket: &Path,
    graceful_timeout: Duration,
    death_poll_timeout: Duration,
    death_poll_interval: Duration,
) -> Result<Outcome> {
    let prior_pid = daemon::read_lockfile_pid(socket);

    if !daemon::lock_is_held(socket) {
        // Nobody holds the lock -- whatever daemon last ran here is already
        // gone (crash, kill -9, reboot). Only stale files can remain; there
        // is nothing left alive to signal.
        let cleaned_socket = remove_if_exists(socket);
        let cleaned_lockfile = remove_if_exists(&daemon::lockfile_path(socket));
        return Ok(Outcome::AlreadyDead { cleaned_socket, cleaned_lockfile });
    }

    if try_graceful_shutdown(socket, graceful_timeout) {
        return Ok(Outcome::Graceful);
    }

    // The lock is held (something is alive), but the graceful path never
    // got a response in time. `prior_pid` came from the lockfile *before* we
    // decided the daemon was unresponsive -- that's the pid the flock is
    // actually pinned to, so it's still the right target even though we
    // can't re-derive it now (a daemon that's genuinely wedged won't answer
    // a fresh IPC request asking it to identify itself either).
    //
    // BUG B: before FIX A, a losing daemon's `File::create` truncated this
    // exact lockfile out from under the live daemon, so `prior_pid` could be
    // `None` even though a daemon is demonstrably alive (we're in this branch
    // *because* the lock is held). FIX A stops that from happening going
    // forward, but a lockfile can still be unreadable for other reasons (hand
    // edited, truncated by something outside Overseer entirely -- the
    // 2026-07-11 incident's original deleter was never identified, see
    // AGENTS.md/NON-GOALS), so this fallback stays regardless: scan the
    // process table for the one process whose argv proves it's *this*
    // socket's daemon, rather than bailing out with a pid the lockfile no
    // longer has.
    let daemon_pid = match prior_pid {
        Some(pid) => pid,
        None => match discover_daemon_pid(socket) {
            DaemonScan::Found(pid) => pid,
            DaemonScan::NotFound => anyhow::bail!(
                "daemon at {} is unresponsive, its lockfile has no readable pid, and no process was \
                 found running `daemon --socket {}` -- refusing to force-kill blindly",
                socket.display(),
                socket.display()
            ),
            DaemonScan::Ambiguous(pids) => anyhow::bail!(
                "daemon at {} is unresponsive, its lockfile has no readable pid, and multiple processes \
                 matched `daemon --socket {}` ({pids:?}) -- refusing to guess which one to kill",
                socket.display(),
                socket.display()
            ),
        },
    };

    let children = direct_children(daemon_pid);
    for pid in &children {
        kill_pid(*pid);
    }
    kill_pid(daemon_pid);
    wait_for_death(daemon_pid, death_poll_timeout, death_poll_interval);

    remove_if_exists(socket);
    remove_if_exists(&daemon::lockfile_path(socket));

    Ok(Outcome::Forced { daemon_pid, reclaimed_children: children.len() })
}

/// Attempts `Request::Shutdown` over IPC, retrying with backoff until
/// `overall_timeout` elapses. Retries exist for one specific, narrow race —
/// the lockfile can be held a few milliseconds before the socket is actually
/// bound (`daemon::run_daemon` acquires the lock, *then* binds), so an
/// immediate connect failure right after `lock_is_held` returns true doesn't
/// necessarily mean the daemon is wedged, just still starting up. A
/// connection that *does* succeed but never reads a response consumes the
/// remaining budget in one wait, since retrying a truly wedged daemon can't
/// help — this returns `false` once time runs out either way.
fn try_graceful_shutdown(socket: &Path, overall_timeout: Duration) -> bool {
    let deadline = Instant::now() + overall_timeout;
    let mut retry_delay = Duration::from_millis(50);

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match client::send_with_timeout(socket, &Request::Shutdown, remaining) {
            Ok(resp) => return resp.ok,
            Err(_) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return false;
                }
                std::thread::sleep(retry_delay.min(remaining));
                retry_delay = (retry_delay * 2).min(Duration::from_millis(500));
            }
        }
    }
}

fn remove_if_exists(path: &Path) -> bool {
    if path.exists() {
        std::fs::remove_file(path).is_ok()
    } else {
        false
    }
}

fn kill_pid(pid: i32) {
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
}

fn pid_is_alive(pid: i32) -> bool {
    let ret = unsafe { libc::kill(pid, 0) };
    // EPERM still means "alive, just not ours to signal again" -- treat it
    // the same as a confirmed-alive 0 return rather than as "gone".
    ret == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn wait_for_death(pid: i32, timeout: Duration, poll_interval: Duration) {
    let deadline = Instant::now() + timeout;
    while pid_is_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(poll_interval);
    }
}

/// Best-effort discovery of `parent_pid`'s direct children via `ps` — there
/// is no portable libc-only way to enumerate a process's children on
/// macOS/Linux without walking `/proc` (Linux-only) or the BSD `kinfo_proc`
/// sysctl tables directly. Shells out to `/bin/ps` by absolute path rather
/// than a `$PATH`-resolved `ps` (mirrors `SessionManager::kill`'s rationale
/// for calling `libc::kill` directly instead of shelling to a `kill`
/// binary — SECURITY-AUDIT.md F6): the pid is what matters here, not a
/// hijackable lookup. Returns an empty list on any failure (missing binary,
/// unexpected output) rather than erroring the whole kill — the daemon pid
/// itself still gets killed either way; this is only the orphan-reclaim
/// half, and finding zero children is themselves a legitimate answer.
///
/// Why direct children are the right thing to hunt for at all: each agent's
/// PTY child is spawned by the terminal-backend crate `session/pty.rs` owns
/// (see its own module doc), which calls `setsid()` in the forked child
/// before exec (confirmed by reading that crate's unix pty backend source)
/// -- it becomes its own session/process-group leader, detached from the
/// daemon's own session. `setsid` doesn't touch the parent/child
/// relationship though, only the session/group, so `ppid` still points at
/// the daemon for as long as the daemon is alive. That's exactly why
/// `AGENTS.md`'s "Daemon death is total" can't be taken as a guarantee
/// against a *forceful* kill the way it is against a graceful one:
/// `SessionManager::kill`'s own doc comment notes real agents (Claude Code,
/// observed) don't reliably die from the PTY hangup a daemon's exit would
/// otherwise deliver. A `SIGKILL` aimed only at the daemon pid leaves those
/// children orphaned but alive; finding them by ancestry and killing them
/// individually is what actually reclaims them.
fn direct_children(parent_pid: i32) -> Vec<i32> {
    let Ok(output) = std::process::Command::new("/bin/ps").args(["-axo", "pid,ppid"]).output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid: i32 = fields.next()?.parse().ok()?;
            let ppid: i32 = fields.next()?.parse().ok()?;
            (ppid == parent_pid).then_some(pid)
        })
        .collect()
}

/// The result of scanning the process table for `socket`'s own daemon
/// process (FIX B's fallback for when the lockfile's recorded pid isn't
/// usable). Zero or multiple matches are both refusals to act, not errors on
/// their own -- `run_with_timeouts` is what turns them into a bailout, this
/// type just reports what was found.
#[derive(Debug, PartialEq, Eq)]
enum DaemonScan {
    Found(i32),
    NotFound,
    Ambiguous(Vec<i32>),
}

/// Runs the same `ps` discovery `direct_children` uses (`/bin/ps` by
/// absolute path, not `$PATH`-resolved -- SECURITY-AUDIT.md F6) to find
/// `socket`'s own daemon process, and hands the raw output to the pure
/// parser (`find_daemon_pid_in_ps_output`) below. `-ww` disables BSD `ps`'s
/// terminal-width truncation of the command column -- without it a long
/// socket path could get cut off mid-string and silently stop matching.
fn discover_daemon_pid(socket: &Path) -> DaemonScan {
    let Ok(output) =
        std::process::Command::new("/bin/ps").args(["-axwwo", "pid=,command="]).output()
    else {
        return DaemonScan::NotFound;
    };
    if !output.status.success() {
        return DaemonScan::NotFound;
    }
    find_daemon_pid_in_ps_output(&String::from_utf8_lossy(&output.stdout), socket)
}

/// Pure parser (no process spawning, no I/O) for `ps -axwwo pid=,command=`
/// output (one process per line: pid, then its full command line, no
/// header since each column name ends in `=`): finds the process whose argv
/// contains the exact contiguous sequence `daemon --socket <socket>`.
///
/// Matching the full three-token sequence -- not a bare substring search,
/// and not just the binary name -- matters for two reasons: a daemon serving
/// a *different* socket must never match just because it also happens to be
/// an `overseer` process (binary-name-only matching would find every
/// `overseer` process on the machine, TUIs included), and a socket path that
/// happens to be a prefix/suffix of another socket's path must not
/// cross-match on a plain substring search of the whole command line.
/// Requiring `daemon`, `--socket`, and the exact path as three consecutive
/// tokens rules out both.
fn find_daemon_pid_in_ps_output(ps_output: &str, socket: &Path) -> DaemonScan {
    let socket_str = socket.to_string_lossy();
    let mut matches = Vec::new();

    for line in ps_output.lines() {
        let mut fields = line.split_whitespace();
        let Some(pid_str) = fields.next() else { continue };
        let Ok(pid) = pid_str.parse::<i32>() else { continue };
        let tokens: Vec<&str> = fields.collect();
        let is_match = tokens
            .windows(3)
            .any(|w| w[0] == "daemon" && w[1] == "--socket" && w[2] == socket_str);
        if is_match {
            matches.push(pid);
        }
    }

    match matches.len() {
        1 => DaemonScan::Found(matches[0]),
        0 => DaemonScan::NotFound,
        _ => DaemonScan::Ambiguous(matches),
    }
}

/// CLI entry point: runs the forceful-cleanup flow against `socket` and
/// prints a one-line summary of what happened, matching the other
/// subcommands' style (`overseer shutdown`, `overseer drop`, etc. print
/// their raw JSON response; this one has no single `Response` to print since
/// it may never reach the daemon at all).
pub fn run_kill(socket: PathBuf) -> Result<()> {
    match run(&socket)? {
        Outcome::AlreadyDead { cleaned_socket, cleaned_lockfile } => {
            println!("no daemon running at {}", socket.display());
            if cleaned_socket || cleaned_lockfile {
                println!("cleaned up stale files left behind by a previous daemon");
            }
        }
        Outcome::Graceful => {
            println!("daemon at {} shut down gracefully", socket.display());
        }
        Outcome::Forced { daemon_pid, reclaimed_children } => {
            print!("daemon at {} did not respond in time -- force-killed pid {daemon_pid}", socket.display());
            if reclaimed_children > 0 {
                println!(" and reclaimed {reclaimed_children} orphaned agent process(es)");
            } else {
                println!();
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write as _;
    use std::os::fd::AsRawFd;

    fn unique_socket(name: &str) -> PathBuf {
        let id = &uuid::Uuid::new_v4().to_string()[..8];
        PathBuf::from(format!("/tmp/ovsr-kill-{name}-{id}")).join("d.sock")
    }

    /// Creates the socket's parent dir and writes `pid` into its lockfile,
    /// under a real `flock` held by the returned `File` — simulating "a
    /// daemon with this pid is alive and holds the lock" without needing a
    /// real `overseer daemon` process. Dropping the returned handle releases
    /// the lock, simulating that daemon dying.
    fn simulate_daemon_holding_lock(socket: &Path, pid: i32) -> File {
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let path = daemon::lockfile_path(socket);
        let file = File::create(&path).unwrap();
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(ret, 0, "test setup must be able to acquire its own fresh lockfile");
        let mut writer = &file;
        write!(writer, "{pid}").unwrap();
        file
    }

    fn cleanup(socket: &Path) {
        let _ = std::fs::remove_dir_all(socket.parent().unwrap());
    }

    // ── already dead ──────────────────────────────────────────────────────────

    #[test]
    fn already_dead_when_no_lockfile_ever_existed() {
        let socket = unique_socket("nolock");
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();

        let outcome = run(&socket).unwrap();
        assert!(matches!(outcome, Outcome::AlreadyDead { .. }));

        cleanup(&socket);
    }

    #[test]
    fn already_dead_cleans_up_a_stale_socket_left_by_a_crashed_daemon() {
        let socket = unique_socket("stalesock");
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        // A crashed daemon leaves the socket file on disk (closing the fd
        // does not unlink it) with nobody holding the lockfile's flock.
        std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert!(socket.exists());

        let outcome = run(&socket).unwrap();
        match outcome {
            Outcome::AlreadyDead { cleaned_socket, .. } => {
                assert!(cleaned_socket, "the stale socket file must be removed")
            }
            other => panic!("expected AlreadyDead, got {other:?}"),
        }
        assert!(!socket.exists(), "stale socket must actually be gone from disk");

        cleanup(&socket);
    }

    #[test]
    fn already_dead_never_signals_the_recycled_pid_recorded_in_a_stale_lockfile() {
        // The lockfile can name a pid that's no longer the daemon (recycled
        // by an unrelated process) once nobody holds the flock -- run()
        // must not act on that number at all, only on the flock state.
        let socket = unique_socket("recycled");
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        std::fs::write(daemon::lockfile_path(&socket), "1").unwrap(); // pid 1 is never ours to touch

        let outcome = run(&socket).unwrap();
        assert!(matches!(outcome, Outcome::AlreadyDead { .. }));

        cleanup(&socket);
    }

    // ── graceful ─────────────────────────────────────────────────────────────

    #[test]
    fn graceful_when_the_daemon_answers_shutdown_in_time() {
        let socket = unique_socket("graceful");
        let ctx = std::sync::Arc::new(crate::ipc::AppCtx {
            registry: std::sync::Arc::new(crate::agent::AgentRegistry::new()),
            sessions: std::sync::Arc::new(crate::session::SessionManager::dry_run()),
            socket: socket.clone(),
            git: std::sync::Arc::new(crate::git::GitClient::dry_run()),
            config: std::sync::Arc::new(crate::config::Config::default()),
            watch_sessions: false,
            shutdown_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
        });
        // `serve_blocking` alone doesn't take the daemon lock -- only
        // `daemon::run_daemon` does that. Hold it ourselves so
        // `lock_is_held` sees exactly what it would against a real daemon.
        let _lock = simulate_daemon_holding_lock(&socket, std::process::id() as i32);
        let socket_clone = socket.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = crate::ipc::serve_blocking(ctx, socket_clone, Some(ready_tx));
        });
        ready_rx.recv().expect("test server failed to start");

        let outcome = run_with_timeouts(
            &socket,
            Duration::from_secs(5),
            Duration::from_millis(500),
            Duration::from_millis(20),
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Graceful), "expected Graceful, got {outcome:?}");

        cleanup(&socket);
    }

    // ── forced ───────────────────────────────────────────────────────────────

    /// Simulates a daemon that's alive (holds the lock) but completely
    /// unreachable over IPC (nothing is listening on the socket at all —
    /// the sharpest version of "never gets a response"). The fake "daemon"
    /// is a real process tree (`sh` plus two backgrounded `sleep`s) so this
    /// also exercises real orphan reclaim via `direct_children`, not just
    /// the top-level kill.
    #[test]
    fn forced_kill_reclaims_the_daemon_and_its_orphaned_children() {
        let socket = unique_socket("forced");
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();

        let mut fake_daemon = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 60 & sleep 60 & wait")
            .spawn()
            .expect("failed to spawn fake daemon process for the test");
        let fake_daemon_pid = fake_daemon.id() as i32;
        // Give the shell a moment to actually fork its two background
        // children before `direct_children` goes looking for them.
        std::thread::sleep(Duration::from_millis(150));

        // The test process is this child's real parent (unlike a real
        // `overseer kill`, run from a separate process from the daemon it
        // targets), so a SIGKILLed fake_daemon sits as a zombie -- still
        // "alive" to `kill(pid, 0)` -- until reaped. Reap concurrently on a
        // background thread so `pid_is_alive` below reflects reality rather
        // than an artifact of this test being its own parent.
        let reaper = std::thread::spawn(move || {
            let _ = fake_daemon.wait();
        });

        let _lock = simulate_daemon_holding_lock(&socket, fake_daemon_pid);
        assert!(!socket.exists(), "nothing ever binds this socket in this test -- IPC must be unreachable");

        let outcome = run_with_timeouts(
            &socket,
            Duration::from_millis(300),
            Duration::from_secs(2),
            Duration::from_millis(20),
        )
        .unwrap();

        match outcome {
            Outcome::Forced { daemon_pid, reclaimed_children } => {
                assert_eq!(daemon_pid, fake_daemon_pid);
                assert!(reclaimed_children >= 1, "expected at least one reclaimed orphan, got {reclaimed_children}");
            }
            other => panic!("expected Forced, got {other:?}"),
        }

        reaper.join().expect("reaper thread panicked");
        assert!(!pid_is_alive(fake_daemon_pid), "the fake daemon process must actually be dead");
        assert!(!daemon::lockfile_path(&socket).exists(), "the lockfile must be cleaned up");

        cleanup(&socket);
    }

    #[test]
    fn forced_kill_errors_out_rather_than_guessing_a_pid_it_never_had() {
        // An empty/unparseable lockfile content while the lock is
        // nonetheless held is a degenerate state this must refuse to act
        // blindly on. BUG B: this is the exact pre-fix repro -- prior to the
        // FIX B ps-scan fallback, an unreadable lockfile pid here meant an
        // unconditional bail, full stop, with no way to recover the pid at
        // all (this was the state the 2026-07-11 incident got stuck in:
        // BUG A had truncated the live daemon's `daemon.pid`, so `overseer
        // kill` bailed here forever, wedged until a manual `kill -9` + file
        // cleanup). Confirmed live before writing FIX B: `run_with_timeouts`
        // hit exactly this branch and returned `Err` with no attempt at
        // recovery. It must *still* error here today -- this socket has no
        // real `daemon --socket <path>` process behind it for the ps scan to
        // find, so `DaemonScan::NotFound` is the fallback's correct answer,
        // not a regression back to the old unconditional bail.
        let socket = unique_socket("nopid");
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let path = daemon::lockfile_path(&socket);
        let file = File::create(&path).unwrap();
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(ret, 0);
        // Deliberately no pid written -- lockfile exists and is locked, but
        // empty.

        let result = run_with_timeouts(
            &socket,
            Duration::from_millis(200),
            Duration::from_millis(200),
            Duration::from_millis(20),
        );
        assert!(result.is_err(), "must refuse to force-kill without a known pid and no ps match");

        drop(file);
        cleanup(&socket);
    }

    // ── FIX B: find_daemon_pid_in_ps_output (pure ps-parsing fallback) ───────

    #[test]
    fn find_daemon_pid_in_ps_output_one_match() {
        let socket = PathBuf::from("/tmp/ovsr-x/d.sock");
        let ps_output = "  1234 /usr/local/bin/overseer daemon --socket /tmp/ovsr-x/d.sock\n\
                          55 /sbin/launchd\n";
        assert_eq!(find_daemon_pid_in_ps_output(ps_output, &socket), DaemonScan::Found(1234));
    }

    #[test]
    fn find_daemon_pid_in_ps_output_zero_matches() {
        let socket = PathBuf::from("/tmp/ovsr-x/d.sock");
        let ps_output = "  55 /sbin/launchd\n  99 -zsh\n";
        assert_eq!(find_daemon_pid_in_ps_output(ps_output, &socket), DaemonScan::NotFound);
    }

    #[test]
    fn find_daemon_pid_in_ps_output_multiple_matches_is_ambiguous() {
        let socket = PathBuf::from("/tmp/ovsr-x/d.sock");
        let ps_output = "  1234 /usr/local/bin/overseer daemon --socket /tmp/ovsr-x/d.sock\n\
                          5678 /Users/me/target/debug/overseer daemon --socket /tmp/ovsr-x/d.sock\n";
        assert_eq!(
            find_daemon_pid_in_ps_output(ps_output, &socket),
            DaemonScan::Ambiguous(vec![1234, 5678])
        );
    }

    #[test]
    fn find_daemon_pid_in_ps_output_excludes_a_daemon_on_a_different_socket() {
        // A process serving a *different* socket -- including one whose path
        // is a prefix of the one we're looking for -- must never match: only
        // the exact three-token sequence with the exact socket path counts.
        let socket = PathBuf::from("/tmp/ovsr-x/d.sock");
        let ps_output = "  1234 /usr/local/bin/overseer daemon --socket /tmp/ovsr-x/d.sock.bak\n\
                          5678 /usr/local/bin/overseer daemon --socket /tmp/ovsr-y/d.sock\n\
                          9012 /usr/local/bin/overseer daemon --socket /tmp/ovsr-x/d.sock\n";
        assert_eq!(find_daemon_pid_in_ps_output(ps_output, &socket), DaemonScan::Found(9012));
    }

    #[test]
    fn find_daemon_pid_in_ps_output_ignores_a_bare_binary_name_match() {
        // An `overseer` process that isn't a daemon at all (e.g. the TUI, or
        // an `overseer status` one-shot) must never match on binary name
        // alone -- only the full `daemon --socket <path>` argv sequence
        // counts.
        let socket = PathBuf::from("/tmp/ovsr-x/d.sock");
        let ps_output = "  1234 /usr/local/bin/overseer\n  5678 /usr/local/bin/overseer status idle\n";
        assert_eq!(find_daemon_pid_in_ps_output(ps_output, &socket), DaemonScan::NotFound);
    }

    // ── direct_children / pid_is_alive ─────────────────────────────────────────

    #[test]
    fn pid_is_alive_true_for_self() {
        assert!(pid_is_alive(std::process::id() as i32));
    }

    #[test]
    fn direct_children_finds_a_real_spawned_child() {
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let child_pid = child.id() as i32;
        std::thread::sleep(Duration::from_millis(50));

        let children = direct_children(std::process::id() as i32);
        assert!(children.contains(&child_pid), "expected {child_pid} among this test process's children: {children:?}");

        kill_pid(child_pid);
        let _ = child.wait();
    }
}
