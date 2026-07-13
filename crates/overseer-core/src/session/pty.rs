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
use crate::session::keys::TermModes;

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
                // Diagnostic breadcrumb for an otherwise-silent PTY death (a
                // real user report: a root's shell exiting unexpectedly with
                // no on-screen error) — printed to stderr, which the detached
                // daemon redirects to daemon.log, so the next occurrence
                // leaves an actual exit code/signal to look at instead of
                // nothing.
                use std::os::unix::process::ExitStatusExt;
                eprintln!(
                    "overseer: agent {} PTY child exited: {status:?} (code={:?}, signal={:?})",
                    self.id.short(),
                    status.code(),
                    status.signal(),
                );
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
    /// Test-only seam: in dry-run mode there's no real PTY for `write()` to
    /// send bytes to, so it records them here instead — lets tests assert
    /// what would have been typed into the PTY without spawning a real
    /// process. Keyed by agent id, bytes appended in write order.
    #[cfg(any(test, feature = "test-util"))]
    dry_run_writes: Mutex<HashMap<AgentId, Vec<u8>>>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            mode: Mode::Real,
            sessions: Mutex::new(HashMap::new()),
            exits: Arc::new(Mutex::new(Vec::new())),
            current_size: Mutex::new(GridSize { cols: DEFAULT_COLS, lines: DEFAULT_LINES }),
            #[cfg(any(test, feature = "test-util"))]
            dry_run_writes: Mutex::new(HashMap::new()),
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
            #[cfg(any(test, feature = "test-util"))]
            dry_run_writes: Mutex::new(HashMap::new()),
        }
    }

    #[cfg(any(test, feature = "test-util"))]
    pub fn is_dry_run(&self) -> bool {
        matches!(self.mode, Mode::DryRun { .. })
    }

    /// A dry-run manager whose `launch()` always fails — for testing rollback
    /// behavior on launch failure without a real, misconfigured PTY spawn.
    #[cfg(any(test, feature = "test-util"))]
    pub fn dry_run_failing_launch() -> Self {
        Self {
            mode: Mode::DryRun { fail_launch: true, live: None },
            sessions: Mutex::new(HashMap::new()),
            exits: Arc::new(Mutex::new(Vec::new())),
            current_size: Mutex::new(GridSize { cols: DEFAULT_COLS, lines: DEFAULT_LINES }),
            #[cfg(any(test, feature = "test-util"))]
            dry_run_writes: Mutex::new(HashMap::new()),
        }
    }

    /// A dry-run manager that reports only `live` as alive — for testing code
    /// that must distinguish which of several agents' sessions are still up.
    #[cfg(any(test, feature = "test-util"))]
    pub fn dry_run_with_live_sessions(live: HashSet<AgentId>) -> Self {
        Self {
            mode: Mode::DryRun { fail_launch: false, live: Some(live) },
            sessions: Mutex::new(HashMap::new()),
            exits: Arc::new(Mutex::new(Vec::new())),
            current_size: Mutex::new(GridSize { cols: DEFAULT_COLS, lines: DEFAULT_LINES }),
            #[cfg(any(test, feature = "test-util"))]
            dry_run_writes: Mutex::new(HashMap::new()),
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
    ///
    /// Calls `libc::kill` directly rather than shelling out to a `kill`
    /// binary resolved through `$PATH` (SECURITY-AUDIT.md F6) — the pid is
    /// already captured, so the subprocess added nothing but a hijackable
    /// lookup.
    pub fn kill(&self, id: &AgentId) {
        let session = self.sessions.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
        let Some(session) = session else { return };
        let _ = session.channel.send(Msg::Shutdown);
        unsafe {
            libc::kill(session.pid as libc::pid_t, libc::SIGKILL);
        }
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
    ///
    /// A degenerate rect (either dimension 0) is ignored outright, keeping
    /// the previous size: it means "the pane isn't visible right now" (a
    /// terminal reporting 0x0 mid-startup or mid-resize), not a real size an
    /// agent's terminal could ever usefully have. Passing 0 through would do
    /// far worse than render wrong — `Term::resize`'s reflow underflows on a
    /// zero dimension and panics the PTY reader thread, which is the *only*
    /// place `Event::ChildExit` is emitted, silently disabling exit
    /// detection for that agent from then on (a real incident: a workspace
    /// stuck `idle` forever after the user typed `exit` in its shell).
    pub fn resize_all(&self, cols: usize, lines: usize) {
        if cols == 0 || lines == 0 {
            return;
        }
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
    /// keystrokes (Task 3) as well as forwarded mouse-wheel reports and
    /// `overseer prompt`'s scripted writes.
    pub fn write(&self, id: &AgentId, bytes: Vec<u8>) {
        #[cfg(any(test, feature = "test-util"))]
        if matches!(self.mode, Mode::DryRun { .. }) {
            self.dry_run_writes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(id.clone())
                .or_default()
                .extend_from_slice(&bytes);
            return;
        }
        if let Some(session) = self.sessions.lock().unwrap_or_else(|e| e.into_inner()).get(id) {
            Notifier(session.channel.clone()).notify(bytes);
        }
    }

    /// Test-only accessor for `dry_run_writes` — every byte `write()` has
    /// recorded for `id` so far, concatenated in write order. Empty for an id
    /// with no recorded writes (including outside dry-run mode).
    #[cfg(any(test, feature = "test-util"))]
    pub fn dry_run_written_bytes(&self, id: &AgentId) -> Vec<u8> {
        self.dry_run_writes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    /// Briefly locks the selected agent's `Term` for rendering. Private —
    /// this is the one seam that may touch `AgentTerm` directly; every public
    /// method on `SessionManager` built from it (`display_offset`,
    /// `grid_snapshot`, `term_modes`) only ever hands out backend-neutral
    /// types, which is what keeps `alacritty_terminal` from crossing this
    /// module's boundary.
    fn with_term<R>(&self, id: &AgentId, f: impl FnOnce(&AgentTerm) -> R) -> Option<R> {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let session = sessions.get(id)?;
        let term = session.term.lock();
        Some(f(&term))
    }

    /// The neutral `TermModes` mock mode's local `Term` currently has set —
    /// the `with_term`-backed twin of `App::term_modes`'s daemon branch,
    /// which instead derives the same struct from a streamed `GridSnapshot`'s
    /// mode bools. `Default` (all `false`) for an unknown id.
    pub fn term_modes(&self, id: &AgentId) -> TermModes {
        self.with_term(id, |term| {
            let mode = term.mode();
            TermModes {
                app_cursor: mode.contains(TermMode::APP_CURSOR),
                bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
                mouse_reporting: mode.intersects(TermMode::MOUSE_MODE),
                sgr_mouse: mode.contains(TermMode::SGR_MOUSE),
                utf8_mouse: mode.contains(TermMode::UTF8_MOUSE),
            }
        })
        .unwrap_or_default()
    }

    /// Scrolls `id`'s terminal history — positive `delta` moves up (further
    /// into scrollback), negative moves down (toward live). A no-op for an
    /// unknown id (already dropped, or dry-run mode, where no session is
    /// ever inserted).
    ///
    /// Returns whether the display offset actually moved. `false` covers the
    /// unknown-id no-op *and* a scroll already clamped at either end (at the
    /// live bottom scrolling down, at the top of history scrolling up) — the
    /// IPC scroll handler uses this to skip pushing a grid that would be
    /// byte-identical to what the client already has (a held-down wheel at
    /// the clamp used to stream full ~1MB snapshots per notch for no visual
    /// change).
    pub fn scroll_display(&self, id: &AgentId, delta: i32) -> bool {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(session) = sessions.get(id) {
            let mut term = session.term.lock();
            let before = term.grid().display_offset();
            term.scroll_display(Scroll::Delta(delta));
            term.grid().display_offset() != before
        } else {
            false
        }
    }

    /// Jumps `id`'s terminal back to the live bottom — same no-op rules and
    /// same "did the offset move" return contract as `scroll_display`.
    pub fn scroll_to_bottom(&self, id: &AgentId) -> bool {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(session) = sessions.get(id) {
            let mut term = session.term.lock();
            let before = term.grid().display_offset();
            term.scroll_display(Scroll::Bottom);
            before != 0
        } else {
            false
        }
    }

    /// How far `id`'s terminal is currently scrolled up from the live bottom
    /// (`0` = live). `0` for an unknown id too — nothing to report either way.
    /// Part of `SessionManager`'s documented public contract even though the
    /// pane renderer now reads the same value off `GridSnapshot::display_offset`
    /// instead of calling this directly — kept public and exercised by this
    /// module's own scroll tests.
    #[allow(dead_code)]
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
    ///
    /// Also sweeps for sessions whose event-loop thread *finished without
    /// ever recording an exit* — a reader that died abnormally (e.g. a panic
    /// inside the terminal emulator) rather than breaking cleanly after
    /// `Event::ChildExit`. That thread is the only source of exit events, so
    /// without this check such an agent would look alive forever no matter
    /// what its process does (the incident behind `resize_all`'s
    /// degenerate-size guard). Reported as an unclean exit: the session is
    /// unusable and the real exit status is unknowable.
    pub fn drain_exits(&self) -> Vec<(AgentId, bool)> {
        {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let mut exits = self.exits.lock().unwrap_or_else(|e| e.into_inner());
            for (id, session) in sessions.iter() {
                // `alive` flips false (on the reader thread) strictly before
                // that thread can finish, so a finished reader with `alive`
                // still true never races a normal exit's own bookkeeping —
                // it can only mean the exit event was never sent at all.
                if session.alive.load(Ordering::Relaxed)
                    && session.reader.as_ref().is_some_and(|r| r.is_finished())
                {
                    session.alive.store(false, Ordering::Relaxed);
                    exits.push((id.clone(), false));
                }
            }
        }
        std::mem::take(&mut *self.exits.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Test-only: stops `id`'s PTY event loop without the `ChildExit`
    /// bookkeeping ever running — the same observable state a reader-thread
    /// panic leaves behind (thread finished, `alive` still true, no exit
    /// recorded), for exercising `drain_exits`'s dead-reader sweep
    /// deterministically.
    #[cfg(any(test, feature = "test-util"))]
    pub fn simulate_reader_death(&self, id: &AgentId) {
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(session) = sessions.get(id) {
            let _ = session.channel.send(Msg::Shutdown);
        }
    }

    /// Test-only: pretend `id`'s PTY child exited with the given exit status,
    /// for exercising `drain_exits()` consumers without spawning a real process.
    #[cfg(any(test, feature = "test-util"))]
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
/// content, for `ui::term_pane::paint_grid_snapshot` to paint — the only path
/// out of this module for terminal content, since a live `Term` can't cross
/// the daemon's process boundary.
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
        mouse_reporting_mode: mode.intersects(TermMode::MOUSE_MODE),
        sgr_mouse_mode: mode.contains(TermMode::SGR_MOUSE),
        utf8_mouse_mode: mode.contains(TermMode::UTF8_MOUSE),
        display_offset: term.grid().display_offset(),
    }
}

/// Pure — maps an alacritty color to `ColorDto`, the wire-neutral twin of
/// `ui::term_pane::map_dto_color`'s reverse mapping. Targets `ColorDto`
/// instead of `ratatui::style::Color` so `session::pty` never needs a
/// `ratatui` dependency.
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

/// Test-support only: the "bytes → `GridSnapshot`" helper that lets
/// `ui::term_pane`'s tests assert against painted `GridSnapshot`s (same
/// fidelity as feeding a real `Term`) without an `alacritty_terminal` import
/// of their own. Feeds `bytes` through a throwaway `Term`, applies a scroll
/// (`Scroll::Delta(scroll_delta)`, then `Scroll::Bottom` if `then_bottom`),
/// and returns the resulting snapshot.
#[cfg(any(test, feature = "test-util"))]
pub fn snapshot_from_bytes_scrolled(
    cols: usize,
    lines: usize,
    bytes: &[u8],
    scroll_delta: i32,
    then_bottom: bool,
) -> GridSnapshot {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::vte::ansi::Processor;

    let size = GridSize { cols, lines };
    let mut term = Term::new(TermConfig::default(), &size, VoidListener);
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, bytes);
    if scroll_delta != 0 {
        term.scroll_display(Scroll::Delta(scroll_delta));
    }
    if then_bottom {
        term.scroll_display(Scroll::Bottom);
    }
    grid_snapshot_from_term(&term)
}

/// Test-support only: `snapshot_from_bytes_scrolled` with no scrolling — the
/// live/unscrolled snapshot.
#[cfg(any(test, feature = "test-util"))]
pub fn snapshot_from_bytes(cols: usize, lines: usize, bytes: &[u8]) -> GridSnapshot {
    snapshot_from_bytes_scrolled(cols, lines, bytes, 0, false)
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
    fn dry_run_write_is_recorded_and_readable_via_the_test_seam() {
        let s = SessionManager::dry_run();
        let id = AgentId::new();
        s.write(&id, b"claude ".to_vec());
        s.write(&id, b"--flag\n".to_vec());
        assert_eq!(s.dry_run_written_bytes(&id), b"claude --flag\n".to_vec());
    }

    #[test]
    fn dry_run_written_bytes_empty_for_an_id_with_no_writes() {
        let s = SessionManager::dry_run();
        assert!(s.dry_run_written_bytes(&AgentId::new()).is_empty());
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
        assert!(s.scroll_display(&id, 10), "moving into real history must report movement");
        assert_eq!(s.display_offset(&id), 10);
        assert!(s.scroll_display(&id, 5));
        assert_eq!(s.display_offset(&id), 15);
        assert!(s.scroll_to_bottom(&id), "leaving a scrolled position must report movement");
        assert_eq!(s.display_offset(&id), 0);

        s.kill(&id);
    }

    // ── degenerate resize / dead-reader sweep (the stuck-workspace incident) ──

    /// Regression test: `resize_all` with a zero dimension (a terminal
    /// reporting a degenerate size mid-startup/mid-resize, forwarded verbatim
    /// by the TUI's `Request::Resize`) used to reach `Term::resize`, whose
    /// reflow underflows on 0 and panics the PTY reader thread — the only
    /// emitter of `Event::ChildExit`, so the agent's exit could never be
    /// detected again. The degenerate resize must be ignored (previous size
    /// kept), the session must stay fully alive, and a later real resize must
    /// still apply.
    #[test]
    fn resize_all_ignores_a_degenerate_zero_size_and_the_session_survives() {
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

        s.resize_all(0, 0);
        s.resize_all(0, 40);
        s.resize_all(120, 0);

        // Give a would-be panic on the reader thread a moment to surface.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(s.is_alive(&id), "a degenerate resize must not kill the session");
        assert!(s.drain_exits().is_empty(), "no exit may be recorded for a live session");

        let grid = s.grid_snapshot(&id).expect("session must still render");
        assert_eq!((grid.cols, grid.lines), (DEFAULT_COLS as u16, DEFAULT_LINES as u16),
            "a degenerate resize must keep the previous size");

        s.resize_all(100, 30);
        let grid = s.grid_snapshot(&id).expect("session must still render");
        assert_eq!((grid.cols, grid.lines), (100, 30), "a real resize must still apply");

        s.kill(&id);
    }

    /// The `bool` return is what lets the IPC scroll handler skip pushing a
    /// grid that would be identical to what the client already has — a
    /// held-down wheel at the clamp used to stream a full snapshot per notch
    /// for zero visual change. Pin the contract at both clamps.
    #[test]
    fn scroll_at_the_clamp_reports_no_movement() {
        let s = SessionManager::new();
        let id = AgentId::new();
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "i=0; while [ $i -lt 100 ]; do echo \"line $i\"; i=$((i+1)); done; sleep 60"]);
        s.launch(id.clone(), &PathBuf::from("/tmp"), &cmd, &HashMap::new()).unwrap();

        let became_dirty = (0..50).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            s.generation(&id).unwrap_or(0) > 0
        });
        assert!(became_dirty, "session must have produced output to scroll through");

        // Already at the live bottom: scrolling further down (or jumping to
        // the bottom) moves nothing.
        assert!(!s.scroll_display(&id, -3), "scrolling down at the live bottom must report no movement");
        assert!(!s.scroll_to_bottom(&id), "scroll_to_bottom at the bottom must report no movement");

        // Way past the top of history: the offset clamps, so a second huge
        // scroll up moves nothing either.
        assert!(s.scroll_display(&id, 1_000_000), "the first jump toward the top is real movement");
        let top = s.display_offset(&id);
        assert!(top > 0);
        assert!(!s.scroll_display(&id, 1_000_000), "scrolling up while clamped at the top must report no movement");
        assert_eq!(s.display_offset(&id), top);

        s.kill(&id);
    }

    /// The incident end to end: a degenerate resize arrives (previously
    /// killing the reader thread), then the user types `exit` — the child's
    /// clean exit must still be detected and reported via `drain_exits`, or
    /// the workspace sits `idle` forever.
    #[test]
    fn exit_is_still_detected_after_a_degenerate_resize() {
        let s = SessionManager::new();
        let id = AgentId::new();
        // An interactive shell reading commands from the PTY, like a
        // workspace's own bare shell.
        let cmd = Command::new("/bin/sh");
        s.launch(id.clone(), &PathBuf::from("/tmp"), &cmd, &HashMap::new()).unwrap();

        s.resize_all(0, 0);
        s.write(&id, b"exit\r".to_vec());

        let mut exits = Vec::new();
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            exits = s.drain_exits();
            if !exits.is_empty() {
                break;
            }
        }
        assert_eq!(exits, vec![(id.clone(), true)],
            "typing exit after a degenerate resize must still be detected as a clean exit");
        assert!(!s.is_alive(&id));

        s.kill(&id);
    }

    /// Defense-in-depth regression test: if the event-loop thread ever dies
    /// *without* recording an exit (a panic inside the terminal emulator —
    /// exactly what the pre-guard degenerate resize did), `drain_exits` must
    /// report the session as an unclean exit rather than leave the agent
    /// looking alive forever.
    #[test]
    fn drain_exits_reports_a_dead_reader_thread_as_an_unclean_exit() {
        let s = SessionManager::new();
        let id = AgentId::new();
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 60"]);
        s.launch(id.clone(), &PathBuf::from("/tmp"), &cmd, &HashMap::new()).unwrap();
        assert!(s.is_alive(&id));

        s.simulate_reader_death(&id);

        let mut exits = Vec::new();
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            exits = s.drain_exits();
            if !exits.is_empty() {
                break;
            }
        }
        assert_eq!(exits, vec![(id.clone(), false)],
            "a dead reader thread must surface as an unclean exit");
        assert!(!s.is_alive(&id), "the session must not look alive after its reader died");

        s.kill(&id);
    }

    #[test]
    fn scroll_display_and_scroll_to_bottom_report_no_movement_for_an_unknown_id() {
        let s = SessionManager::dry_run();
        assert!(!s.scroll_display(&AgentId::new(), 5));
        assert!(!s.scroll_to_bottom(&AgentId::new()));
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
    fn grid_snapshot_reports_mouse_modes_requested_by_inner_tui() {
        let snapshot = snapshot_from_bytes(80, 24, b"\x1b[?1000h\x1b[?1006h");
        assert!(snapshot.mouse_reporting_mode);
        assert!(snapshot.sgr_mouse_mode);
        assert!(!snapshot.utf8_mouse_mode);

        let snapshot = snapshot_from_bytes(80, 24, b"\x1b[?1003h\x1b[?1005h");
        assert!(snapshot.mouse_reporting_mode);
        assert!(!snapshot.sgr_mouse_mode);
        assert!(snapshot.utf8_mouse_mode);

        let snapshot = snapshot_from_bytes(80, 24, b"\x1b[?1000h\x1b[?1000l");
        assert!(!snapshot.mouse_reporting_mode);
    }

    #[test]
    fn dto_color_maps_named_and_bright_and_rgb() {
        assert_eq!(dto_color(AnsiColor::Named(NamedColor::Green)), ColorDto::Green);
        assert_eq!(dto_color(AnsiColor::Named(NamedColor::BrightBlack)), ColorDto::DarkGray);
        assert_eq!(dto_color(AnsiColor::Indexed(200)), ColorDto::Indexed(200));
        assert_eq!(dto_color(AnsiColor::Spec(Rgb { r: 1, g: 2, b: 3 })), ColorDto::Rgb(1, 2, 3));
    }
}
