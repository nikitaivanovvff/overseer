use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use alacritty_terminal::event::{Event, EventListener, Notify, OnResize, WindowSize};
use alacritty_terminal::event_loop::{
    EventLoop, EventLoopSender, Msg, Notifier, State as EventLoopState,
};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::tty::{self, Options as PtyOptions, Pty, Shell};

use crate::agent::AgentId;

/// Scrollback cap (PHASE6.md §2) — bounds memory for long-running agents.
const SCROLLBACK_LINES: usize = 10_000;
/// Grid size used for a session launched before the UI has ever reported a
/// real pane rect (overwritten by the first `resize_all`).
const DEFAULT_COLS: usize = 80;
const DEFAULT_LINES: usize = 24;

/// Every agent PTY is sized to the single, shared live-pane rect (PHASE6.md
/// §2: "Uniform PTY size") — there are no per-agent sizes.
#[derive(Clone, Copy)]
pub struct GridSize {
    pub cols: usize,
    pub lines: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

pub type AgentTerm = Term<EventProxy>;

/// `EventListener` for one agent's `Term`. Handles the query-reply loop
/// (`PtyWrite`) and child-exit bookkeeping invisibly to the rest of the app —
/// callers only ever see `SessionManager::drain_exits`/`is_alive`.
#[derive(Clone)]
pub struct EventProxy {
    id: AgentId,
    sender: Arc<OnceLock<EventLoopSender>>,
    alive: Arc<AtomicBool>,
    exits: Arc<Mutex<Vec<AgentId>>>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => {
                if let Some(sender) = self.sender.get() {
                    Notifier(sender.clone()).notify(text.into_bytes());
                }
            }
            Event::ChildExit(_) => {
                self.alive.store(false, Ordering::Relaxed);
                self.exits.lock().unwrap_or_else(|e| e.into_inner()).push(self.id.clone());
            }
            _ => {}
        }
    }
}

type SessionEventLoop = EventLoop<Pty, EventProxy>;

struct PtySession {
    term: Arc<FairMutex<AgentTerm>>,
    channel: EventLoopSender,
    alive: Arc<AtomicBool>,
    reader: Option<JoinHandle<(SessionEventLoop, EventLoopState)>>,
    /// The child's own pid, captured before it moves into the `EventLoop` —
    /// needed because `kill()` must be able to force-terminate it directly
    /// (see the comment there for why a hangup alone isn't enough).
    pid: u32,
}

enum Mode {
    Real,
    /// Test/`--mock` mode: no PTYs are ever spawned. `live` mirrors the old
    /// `TmuxClient` dry-run knob — `Some(ids)` reports exactly those agents as
    /// alive, `None` reports everything dead.
    DryRun { fail_launch: bool, live: Option<HashSet<AgentId>> },
}

/// Owns every agent's PTY + terminal emulator. Replaces `TmuxClient` as the
/// only terminal-backend boundary (AGENTS.md rule carries over under this
/// name) — sessions are keyed directly by `AgentId`, no session-name mapping.
pub struct SessionManager {
    mode: Mode,
    sessions: Mutex<HashMap<AgentId, PtySession>>,
    exits: Arc<Mutex<Vec<AgentId>>>,
    current_size: Mutex<GridSize>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            mode: Mode::Real,
            sessions: Mutex::new(HashMap::new()),
            exits: Arc::new(Mutex::new(Vec::new())),
            current_size: Mutex::new(GridSize { cols: DEFAULT_COLS, lines: DEFAULT_LINES }),
        }
    }

    /// Returns a no-op manager that spawns no real PTYs — for tests, and for
    /// `--mock` so seeded demo data never launches a real process.
    pub fn dry_run() -> Self {
        Self {
            mode: Mode::DryRun { fail_launch: false, live: None },
            sessions: Mutex::new(HashMap::new()),
            exits: Arc::new(Mutex::new(Vec::new())),
            current_size: Mutex::new(GridSize { cols: DEFAULT_COLS, lines: DEFAULT_LINES }),
        }
    }

    #[cfg(test)]
    pub(crate) fn is_dry_run(&self) -> bool {
        matches!(self.mode, Mode::DryRun { .. })
    }

    /// A dry-run manager whose `launch()` always fails — for testing rollback
    /// behavior on launch failure without a real, misconfigured PTY spawn.
    #[cfg(test)]
    pub fn dry_run_failing_launch() -> Self {
        Self {
            mode: Mode::DryRun { fail_launch: true, live: None },
            sessions: Mutex::new(HashMap::new()),
            exits: Arc::new(Mutex::new(Vec::new())),
            current_size: Mutex::new(GridSize { cols: DEFAULT_COLS, lines: DEFAULT_LINES }),
        }
    }

    /// A dry-run manager that reports only `live` as alive — for testing code
    /// that must distinguish which of several agents' sessions are still up.
    #[cfg(test)]
    pub fn dry_run_with_live_sessions(live: HashSet<AgentId>) -> Self {
        Self {
            mode: Mode::DryRun { fail_launch: false, live: Some(live) },
            sessions: Mutex::new(HashMap::new()),
            exits: Arc::new(Mutex::new(Vec::new())),
            current_size: Mutex::new(GridSize { cols: DEFAULT_COLS, lines: DEFAULT_LINES }),
        }
    }

    /// Spawns `cmd` in a new PTY with `env` injected, keyed by `id`.
    pub fn launch(
        &self,
        id: AgentId,
        cwd: &Path,
        cmd: &Command,
        env: &HashMap<String, String>,
    ) -> Result<()> {
        if let Mode::DryRun { fail_launch, .. } = &self.mode {
            anyhow::ensure!(!fail_launch, "simulated launch failure for '{}'", id.short());
            return Ok(());
        }

        let grid_size = *self.current_size.lock().unwrap_or_else(|e| e.into_inner());

        let mut full_env = env.clone();
        full_env.entry("TERM".to_string()).or_insert_with(|| "xterm-256color".to_string());
        full_env.entry("COLORTERM".to_string()).or_insert_with(|| "truecolor".to_string());

        let program = cmd.get_program().to_string_lossy().to_string();
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();

        let pty_options = PtyOptions {
            shell: Some(Shell::new(program, args)),
            working_directory: Some(cwd.to_path_buf()),
            drain_on_exit: true,
            env: full_env,
            ..PtyOptions::default()
        };

        let window_size = WindowSize {
            num_lines: grid_size.lines as u16,
            num_cols: grid_size.cols as u16,
            cell_width: 0,
            cell_height: 0,
        };

        let sender_slot: Arc<OnceLock<EventLoopSender>> = Arc::new(OnceLock::new());
        let alive = Arc::new(AtomicBool::new(true));
        let proxy = EventProxy {
            id: id.clone(),
            sender: sender_slot.clone(),
            alive: alive.clone(),
            exits: self.exits.clone(),
        };

        let term_config = TermConfig { scrolling_history: SCROLLBACK_LINES, ..TermConfig::default() };
        let term = Arc::new(FairMutex::new(Term::new(term_config, &grid_size, proxy.clone())));

        let pty = tty::new(&pty_options, window_size, 0)
            .with_context(|| format!("failed to spawn pty for agent '{}'", id.short()))?;
        let pid = pty.child().id();

        let event_loop = EventLoop::new(term.clone(), proxy, pty, false, false)
            .with_context(|| format!("failed to start pty event loop for agent '{}'", id.short()))?;
        let channel = event_loop.channel();
        let _ = sender_slot.set(channel.clone());
        let reader = event_loop.spawn();

        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).insert(
            id,
            PtySession { term, channel, alive, reader: Some(reader), pid },
        );
        Ok(())
    }

    /// Kills the agent's PTY and forgets it. Best-effort: killing an unknown
    /// (already-dead) id is not an error — callers rely on this for recursive
    /// drop and the dead-session watcher.
    ///
    /// Sends `SIGKILL` to the child directly rather than relying on the PTY
    /// hangup alone: some agents (observed with real Claude Code) don't die
    /// from a hangup, and joining the reader thread drops the `Pty`, whose
    /// `Drop` impl calls the blocking `Child::wait()` — with a hangup-only
    /// signal, that `wait()` can block this calling thread (the UI thread,
    /// for a `d`/`D`/quit-confirmed kill) forever. `SIGKILL` can't be caught
    /// or ignored, so the child is reliably dead by the time we join.
    pub fn kill(&self, id: &AgentId) {
        let session = self.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
        let Some(session) = session else { return };
        let _ = session.channel.send(Msg::Shutdown);
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &session.pid.to_string()])
            .status();
        if let Some(handle) = session.reader {
            let _ = handle.join();
        }
    }

    pub fn is_alive(&self, id: &AgentId) -> bool {
        match &self.mode {
            Mode::DryRun { live, .. } => live.as_ref().is_some_and(|ids| ids.contains(id)),
            Mode::Real => self
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(id)
                .is_some_and(|s| s.alive.load(Ordering::Relaxed)),
        }
    }

    /// Resizes every live session to `(cols, lines)` and remembers it as the
    /// size new sessions launch at — all agents share one rect (§2).
    pub fn resize_all(&self, cols: usize, lines: usize) {
        *self.current_size.lock().unwrap_or_else(|e| e.into_inner()) = GridSize { cols, lines };
        if matches!(self.mode, Mode::DryRun { .. }) {
            return;
        }
        let window_size = WindowSize {
            num_lines: lines as u16,
            num_cols: cols as u16,
            cell_width: 0,
            cell_height: 0,
        };
        for session in self.sessions.lock().unwrap_or_else(|e| e.into_inner()).values() {
            session.term.lock().resize(GridSize { cols, lines });
            Notifier(session.channel.clone()).on_resize(window_size);
        }
    }

    /// Forwards raw bytes to the agent's PTY — the input path for jump-in
    /// keystrokes (Task 3) as well as this module's own query-reply writes.
    pub fn write(&self, id: &AgentId, bytes: Vec<u8>) {
        if let Some(session) = self.sessions.lock().unwrap_or_else(|e| e.into_inner()).get(id) {
            Notifier(session.channel.clone()).notify(bytes);
        }
    }

    /// Briefly locks the selected agent's `Term` for rendering (Task 2).
    pub fn with_term<R>(&self, id: &AgentId, f: impl FnOnce(&AgentTerm) -> R) -> Option<R> {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let session = sessions.get(id)?;
        let term = session.term.lock();
        Some(f(&term))
    }

    /// Drains the set of agents whose PTY child has exited since the last
    /// call — consumed by the dead-session watcher in place of polling.
    pub fn drain_exits(&self) -> Vec<AgentId> {
        std::mem::take(&mut *self.exits.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Test-only: pretend `id`'s PTY child exited, for exercising
    /// `drain_exits()` consumers without spawning a real process.
    #[cfg(test)]
    pub fn simulate_exit(&self, id: AgentId) {
        self.exits.lock().unwrap_or_else(|e| e.into_inner()).push(id);
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn dry_run_launch_is_noop() {
        let s = SessionManager::dry_run();
        let cmd = Command::new("claude");
        let env = HashMap::new();
        s.launch(AgentId::new(), Path::new("/tmp"), &cmd, &env).unwrap();
    }

    #[test]
    fn dry_run_failing_launch_errors() {
        let s = SessionManager::dry_run_failing_launch();
        let cmd = Command::new("claude");
        let env = HashMap::new();
        assert!(s.launch(AgentId::new(), Path::new("/tmp"), &cmd, &env).is_err());
    }

    #[test]
    fn dry_run_reports_everything_dead_by_default() {
        let s = SessionManager::dry_run();
        assert!(!s.is_alive(&AgentId::new()));
    }

    #[test]
    fn dry_run_with_live_sessions_reports_only_those_alive() {
        let live_id = AgentId::new();
        let dead_id = AgentId::new();
        let s = SessionManager::dry_run_with_live_sessions(
            [live_id.clone()].into_iter().collect(),
        );
        assert!(s.is_alive(&live_id));
        assert!(!s.is_alive(&dead_id));
    }

    #[test]
    fn dry_run_kill_of_unknown_id_is_noop() {
        let s = SessionManager::dry_run();
        s.kill(&AgentId::new());
    }

    #[test]
    fn dry_run_drain_exits_is_always_empty() {
        let s = SessionManager::dry_run();
        assert!(s.drain_exits().is_empty());
    }

    #[test]
    fn dry_run_write_and_resize_do_not_panic() {
        let s = SessionManager::dry_run();
        s.write(&AgentId::new(), b"hello".to_vec());
        s.resize_all(120, 40);
    }

    #[test]
    fn real_launch_and_kill_terminates_the_process() {
        let s = SessionManager::new();
        let id = AgentId::new();
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 60"]);
        s.launch(id.clone(), &PathBuf::from("/tmp"), &cmd, &HashMap::new()).unwrap();
        assert!(s.is_alive(&id));
        s.kill(&id);
        assert!(!s.is_alive(&id), "killed session must be forgotten");
    }

    /// Regression test: some real agents (observed with Claude Code) don't
    /// die from a PTY hangup alone. `kill()` used to join the reader thread
    /// and then drop its `Pty`, whose `Drop` blocks on `Child::wait()` — with
    /// only a hangup, that blocked the *calling* thread (the UI thread, for a
    /// `d`/`D`/quit-confirmed kill) forever if the child ignored it. Runs
    /// `kill()` on a background thread and asserts it completes quickly
    /// rather than actually blocking the test run forever if it regresses.
    #[test]
    fn kill_does_not_block_on_a_child_that_ignores_hangup_and_terminate() {
        let s = Arc::new(SessionManager::new());
        let id = AgentId::new();
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "trap '' HUP TERM; sleep 60 & wait $!"]);
        s.launch(id.clone(), &PathBuf::from("/tmp"), &cmd, &HashMap::new()).unwrap();
        assert!(s.is_alive(&id));

        let (tx, rx) = std::sync::mpsc::channel();
        let (s2, id2) = (s.clone(), id.clone());
        std::thread::spawn(move || {
            s2.kill(&id2);
            let _ = tx.send(());
        });

        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("kill() must not block forever on a child that ignores HUP/TERM");
        assert!(!s.is_alive(&id));
    }
}
