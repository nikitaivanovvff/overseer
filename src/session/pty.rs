use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use alacritty_terminal::event::{Event, EventListener, Notify, OnResize, WindowSize};
use alacritty_terminal::event_loop::{
    EventLoop, EventLoopSender, Msg, Notifier, State as EventLoopState,
};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::tty::{self, Options as PtyOptions, Pty, Shell};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};

use crate::agent::AgentId;
use crate::ipc::protocol::{CellDto, ColorDto, GridSnapshot};

/// Scrollback cap — bounds memory for long-running agents.
const SCROLLBACK_LINES: usize = 10_000;
/// Grid size used for a session launched before the UI has ever reported a
/// real pane rect (overwritten by the first `resize_all`).
const DEFAULT_COLS: usize = 80;
const DEFAULT_LINES: usize = 24;

/// Every agent PTY is sized to the single, shared live-pane rect — there are
/// no per-agent sizes.
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
/// (`PtyWrite`), child-exit bookkeeping, and generation tracking invisibly to
/// the rest of the app — callers only ever see `SessionManager::drain_exits`/
/// `is_alive`/`generation`.
#[derive(Clone)]
pub struct EventProxy {
    id: AgentId,
    sender: Arc<OnceLock<EventLoopSender>>,
    alive: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    exits: Arc<Mutex<Vec<(AgentId, bool)>>>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => {
                if let Some(sender) = self.sender.get() {
                    Notifier(sender.clone()).notify(text.into_bytes());
                }
            }
            Event::ChildExit(status) => {
                self.alive.store(false, Ordering::Relaxed);
                self.exits
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((self.id.clone(), status.success()));
            }
            // "New terminal content available" — bumps a per-session
            // generation counter that each attach connection compares
            // against the last generation *it* sent, to decide whether the
            // watched agent's grid snapshot needs resending (session::pty
            // doesn't have raw PTY bytes to stream directly; see
            // `GridSnapshot`). A counter rather than a consumed flag so two
            // connections watching the same agent each see every update
            // (F3) instead of racing to steal one shared flag.
            Event::Wakeup => {
                self.generation.fetch_add(1, Ordering::Relaxed);
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
    generation: Arc<AtomicU64>,
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
    exits: Arc<Mutex<Vec<(AgentId, bool)>>>,
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
        };

        let window_size = WindowSize {
            num_lines: grid_size.lines as u16,
            num_cols: grid_size.cols as u16,
            cell_width: 0,
            cell_height: 0,
        };

        let sender_slot: Arc<OnceLock<EventLoopSender>> = Arc::new(OnceLock::new());
        let alive = Arc::new(AtomicBool::new(true));
        // Starts at 0 — a `Watch` arriving before the first real Wakeup still
        // gets its content, since `Watch` always sends an immediate snapshot
        // regardless of generation; no need to fake an initial "dirty" value.
        let generation = Arc::new(AtomicU64::new(0));
        let proxy = EventProxy {
            id: id.clone(),
            sender: sender_slot.clone(),
            alive: alive.clone(),
            generation: generation.clone(),
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
            PtySession { term, channel, alive, generation, reader: Some(reader), pid },
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

    /// Scrolls `id`'s terminal history — positive `delta` moves up (further
    /// into scrollback), negative moves down (toward live). A no-op for an
    /// unknown id (already dropped, or dry-run mode, where no session is
    /// ever inserted).
    pub fn scroll_display(&self, id: &AgentId, delta: i32) {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(session) = sessions.get(id) {
            session.term.lock().scroll_display(Scroll::Delta(delta));
        }
    }

    /// Jumps `id`'s terminal back to the live bottom — same no-op rules as
    /// `scroll_display`.
    pub fn scroll_to_bottom(&self, id: &AgentId) {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(session) = sessions.get(id) {
            session.term.lock().scroll_display(Scroll::Bottom);
        }
    }

    /// How far `id`'s terminal is currently scrolled up from the live bottom
    /// (`0` = live). `0` for an unknown id too — nothing to report either way.
    pub fn display_offset(&self, id: &AgentId) -> usize {
        self.with_term(id, |term| term.grid().display_offset()).unwrap_or(0)
    }

    /// Returns `id`'s current content-generation counter — incremented every
    /// time the terminal produces new content (`Event::Wakeup`). A read, not
    /// a consume: each attach connection compares this against the last
    /// generation *it* sent for `id` to decide whether a resend is needed,
    /// so two connections watching the same agent both see every update
    /// instead of racing to steal one shared dirty flag (a real bug,
    /// PERFORMANCE.md F3 — the previous consumed-bool design meant whichever
    /// connection polled first each tick silently starved the other).
    /// `None` for an unknown id or in dry-run mode — nothing to send either
    /// way.
    pub fn generation(&self, id: &AgentId) -> Option<u64> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .map(|s| s.generation.load(Ordering::Relaxed))
    }

    /// Renders `id`'s current terminal grid into a wire-ready `GridSnapshot`
    /// (DAEMON.md "Attach protocol") — `None` if the session isn't live.
    pub fn grid_snapshot(&self, id: &AgentId) -> Option<GridSnapshot> {
        self.with_term(id, grid_snapshot_from_term)
    }

    /// Drains the set of agents whose PTY child has exited since the last call
    /// — consumed by the dead-session watcher in place of polling. Each entry is
    /// `(id, success)`, `success` being the child's exit status (`true` for a
    /// clean exit code 0).
    pub fn drain_exits(&self) -> Vec<(AgentId, bool)> {
        std::mem::take(&mut *self.exits.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Test-only: pretend `id`'s PTY child exited with the given exit status,
    /// for exercising `drain_exits()` consumers without spawning a real process.
    #[cfg(test)]
    pub fn simulate_exit(&self, id: AgentId, success: bool) {
        self.exits.lock().unwrap_or_else(|e| e.into_inner()).push((id, success));
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Pure — extracts a full `GridSnapshot` from `term`'s current renderable
/// content. Mirrors `ui::term_pane::paint_term`'s cell iteration (wide-char
/// spacers stay `None`, same flag/color handling) but builds a wire DTO
/// instead of painting into a ratatui buffer, since the daemon can't hand a
/// live `Term` across the process boundary.
fn grid_snapshot_from_term<T: EventListener>(term: &Term<T>) -> GridSnapshot {
    let cols = term.columns();
    let lines = term.screen_lines();
    let mut cells: Vec<Option<CellDto>> = vec![None; cols * lines];

    let content = term.renderable_content();
    for cell in content.display_iter {
        let point = cell.point;
        if point.line.0 < 0 {
            continue;
        }
        let row = point.line.0 as usize;
        let col = point.column.0;
        if row >= lines || col >= cols || cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let ch = if cell.c == '\0' { ' ' } else { cell.c };
        cells[row * cols + col] = Some(CellDto {
            ch,
            fg: dto_color(cell.fg),
            bg: dto_color(cell.bg),
            bold: cell.flags.contains(Flags::BOLD),
            italic: cell.flags.contains(Flags::ITALIC),
            underline: cell.flags.contains(Flags::UNDERLINE),
            inverse: cell.flags.contains(Flags::INVERSE),
        });
    }

    let cursor_point = content.cursor.point;
    let cursor = if cursor_point.line.0 >= 0 {
        let row = cursor_point.line.0 as usize;
        let col = cursor_point.column.0;
        (row < lines && col < cols).then_some((row as u16, col as u16))
    } else {
        None
    };

    let mode = term.mode();
    GridSnapshot {
        cols: cols as u16,
        lines: lines as u16,
        cells,
        cursor,
        app_cursor_mode: mode.contains(TermMode::APP_CURSOR),
        bracketed_paste_mode: mode.contains(TermMode::BRACKETED_PASTE),
        display_offset: term.grid().display_offset(),
    }
}

/// Pure — the wire-side twin of `ui::term_pane::map_color`, targeting
/// `ColorDto` instead of `ratatui::style::Color` so `session::pty` never
/// needs a `ratatui` dependency.
fn dto_color(color: AnsiColor) -> ColorDto {
    match color {
        AnsiColor::Spec(rgb) => ColorDto::Rgb(rgb.r, rgb.g, rgb.b),
        AnsiColor::Indexed(idx) => ColorDto::Indexed(idx),
        AnsiColor::Named(named) => match named {
            NamedColor::Black => ColorDto::Black,
            NamedColor::Red => ColorDto::Red,
            NamedColor::Green => ColorDto::Green,
            NamedColor::Yellow => ColorDto::Yellow,
            NamedColor::Blue => ColorDto::Blue,
            NamedColor::Magenta => ColorDto::Magenta,
            NamedColor::Cyan => ColorDto::Cyan,
            NamedColor::White => ColorDto::White,
            NamedColor::BrightBlack => ColorDto::DarkGray,
            NamedColor::BrightRed => ColorDto::LightRed,
            NamedColor::BrightGreen => ColorDto::LightGreen,
            NamedColor::BrightYellow => ColorDto::LightYellow,
            NamedColor::BrightBlue => ColorDto::LightBlue,
            NamedColor::BrightMagenta => ColorDto::LightMagenta,
            NamedColor::BrightCyan => ColorDto::LightCyan,
            NamedColor::BrightWhite => ColorDto::White,
            _ => ColorDto::Reset,
        },
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

    // ── generation / grid_snapshot ────────────────────────────────────────────

    #[test]
    fn generation_is_none_for_an_unknown_id() {
        let s = SessionManager::dry_run();
        assert!(s.generation(&AgentId::new()).is_none());
    }

    #[test]
    fn grid_snapshot_is_none_for_an_unknown_id() {
        let s = SessionManager::dry_run();
        assert!(s.grid_snapshot(&AgentId::new()).is_none());
    }

    /// Regression test for PERFORMANCE.md F3: with the old consumed dirty
    /// bool, whichever caller read it first each tick reset it for everyone
    /// else, so a second watcher of the same agent could see `false` forever
    /// even though the terminal kept producing content. `generation` is a
    /// read, not a consume — two independent "watchers" polling it must both
    /// observe the same bumped value, neither stealing it from the other.
    #[test]
    fn generation_is_readable_by_multiple_watchers_without_stealing() {
        let s = SessionManager::new();
        let id = AgentId::new();
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "printf hello; sleep 60"]);
        s.launch(id.clone(), &PathBuf::from("/tmp"), &cmd, &HashMap::new()).unwrap();

        let became_dirty = (0..50).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            s.generation(&id).unwrap_or(0) > 0
        });
        assert!(became_dirty, "session must have produced output first");

        // Simulate two independent connections both watching this agent:
        // both must see the same non-zero generation, and neither read may
        // reset it for the other.
        let watcher_a = s.generation(&id).unwrap();
        let watcher_b = s.generation(&id).unwrap();
        assert_eq!(watcher_a, watcher_b, "both watchers must observe the same generation");
        assert!(watcher_a > 0);
        assert_eq!(s.generation(&id).unwrap(), watcher_a, "reading generation must not consume it");

        s.kill(&id);
    }

    #[test]
    fn real_session_becomes_dirty_and_yields_a_grid_snapshot() {
        let s = SessionManager::new();
        let id = AgentId::new();
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "printf hello; sleep 60"]);
        s.launch(id.clone(), &PathBuf::from("/tmp"), &cmd, &HashMap::new()).unwrap();

        let became_dirty = (0..50).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            s.generation(&id).unwrap_or(0) > 0
        });
        assert!(became_dirty, "a live session that printed output must eventually report dirty");

        let grid = s.grid_snapshot(&id).expect("live session must yield a grid snapshot");
        let text: String = grid
            .cells
            .iter()
            .filter_map(|c| c.as_ref().map(|c| c.ch))
            .collect();
        assert!(text.contains("hello"), "grid should contain the child's printed output, got {text:?}");

        s.kill(&id);
    }

    // ── scroll_display / scroll_to_bottom / display_offset ──────────────────

    #[test]
    fn scroll_display_on_unknown_id_does_not_panic() {
        let s = SessionManager::dry_run();
        s.scroll_display(&AgentId::new(), 5);
    }

    #[test]
    fn scroll_to_bottom_on_unknown_id_does_not_panic() {
        let s = SessionManager::dry_run();
        s.scroll_to_bottom(&AgentId::new());
    }

    #[test]
    fn display_offset_is_zero_for_an_unknown_id() {
        let s = SessionManager::dry_run();
        assert_eq!(s.display_offset(&AgentId::new()), 0);
    }

    #[test]
    fn real_session_scroll_display_changes_offset_and_scroll_to_bottom_resets_it() {
        let s = SessionManager::new();
        let id = AgentId::new();
        // Print more lines than the default 24-line grid holds so there's
        // real scrollback history to move into.
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "i=0; while [ $i -lt 100 ]; do echo \"line $i\"; i=$((i+1)); done; sleep 60"]);
        s.launch(id.clone(), &PathBuf::from("/tmp"), &cmd, &HashMap::new()).unwrap();

        let became_dirty = (0..50).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            s.generation(&id).unwrap_or(0) > 0
        });
        assert!(became_dirty, "session must have produced output to scroll through");

        assert_eq!(s.display_offset(&id), 0, "a fresh session starts at the live bottom");
        s.scroll_display(&id, 10);
        assert_eq!(s.display_offset(&id), 10);
        s.scroll_display(&id, 5);
        assert_eq!(s.display_offset(&id), 15);
        s.scroll_to_bottom(&id);
        assert_eq!(s.display_offset(&id), 0);

        s.kill(&id);
    }

    // ── grid_snapshot_from_term / dto_color (pure) ───────────────────────────

    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::vte::ansi::{NamedColor, Processor, Rgb};

    fn term_from(bytes: &[u8], cols: usize, lines: usize) -> Term<VoidListener> {
        let size = GridSize { cols, lines };
        let mut term = Term::new(TermConfig::default(), &size, VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, bytes);
        term
    }

    #[test]
    fn grid_snapshot_reports_dimensions_and_plain_text() {
        let term = term_from(b"hi", 10, 3);
        let grid = grid_snapshot_from_term(&term);
        assert_eq!(grid.cols, 10);
        assert_eq!(grid.lines, 3);
        assert_eq!(grid.cells[0].as_ref().unwrap().ch, 'h');
        assert_eq!(grid.cells[1].as_ref().unwrap().ch, 'i');
        assert_eq!(grid.cells[2].as_ref().unwrap().ch, ' ');
    }

    #[test]
    fn grid_snapshot_wide_char_spacer_stays_none() {
        // U+4F60 ("你") is double-width — the spacer cell after it must not
        // get a stray second glyph (mirrors term_pane's identical test).
        let term = term_from("你".as_bytes(), 10, 3);
        let grid = grid_snapshot_from_term(&term);
        assert_eq!(grid.cells[0].as_ref().unwrap().ch, '你');
        assert!(grid.cells[1].is_none());
    }

    #[test]
    fn grid_snapshot_captures_bold_and_color_flags() {
        // \x1b[1;31m = bold + red foreground
        let term = term_from(b"\x1b[1;31mX", 10, 3);
        let grid = grid_snapshot_from_term(&term);
        let cell = grid.cells[0].as_ref().unwrap();
        assert_eq!(cell.ch, 'X');
        assert!(cell.bold);
        assert_eq!(cell.fg, ColorDto::Red);
    }

    #[test]
    fn grid_snapshot_cursor_position_reflects_input() {
        let term = term_from(b"ab", 10, 3);
        let grid = grid_snapshot_from_term(&term);
        assert_eq!(grid.cursor, Some((0, 2)));
    }

    #[test]
    fn grid_snapshot_reports_zero_display_offset_at_the_live_bottom() {
        let term = term_from(b"hi", 10, 3);
        assert_eq!(grid_snapshot_from_term(&term).display_offset, 0);
    }

    #[test]
    fn grid_snapshot_reports_nonzero_display_offset_when_scrolled() {
        use alacritty_terminal::grid::Scroll;
        let mut bytes = Vec::new();
        for i in 0..20 {
            bytes.extend_from_slice(format!("line{i}\r\n").as_bytes());
        }
        let mut term = term_from(&bytes, 10, 3);
        term.scroll_display(Scroll::Delta(4));
        assert_eq!(grid_snapshot_from_term(&term).display_offset, 4);
    }

    #[test]
    fn dto_color_maps_named_and_bright_and_rgb() {
        assert_eq!(dto_color(AnsiColor::Named(NamedColor::Green)), ColorDto::Green);
        assert_eq!(dto_color(AnsiColor::Named(NamedColor::BrightBlack)), ColorDto::DarkGray);
        assert_eq!(dto_color(AnsiColor::Indexed(200)), ColorDto::Indexed(200));
        assert_eq!(dto_color(AnsiColor::Spec(Rgb { r: 1, g: 2, b: 3 })), ColorDto::Rgb(1, 2, 3));
    }
}
