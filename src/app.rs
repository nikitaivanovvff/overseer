use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::agent::{AgentId, AgentNode, AgentStatus, AgentTree};
use crate::ipc::protocol::{AgentDto, AttachEvent, GridSnapshot, Request, Response};
use crate::ipc::AppCtx;

/// What a text-input prompt (`n` / `s` / `/`) is being collected for.
#[derive(Debug, Clone)]
pub enum PendingAction {
    SpawnRoot,
    SpawnChild { parent_id: AgentId },
    /// Fuzzy agent search (PHASE5B.md) — unlike the spawn prompts, this one
    /// re-filters the tree live as `buffer` changes rather than waiting for
    /// a submit; `ui::render` reads it directly off `App`'s `input` field.
    Search,
}

/// Active when the user is typing a task description for `n`/`s`.
pub struct InputState {
    pub action: PendingAction,
    pub buffer: String,
}

/// What a y/n confirmation prompt is asking about.
#[derive(Debug, Clone)]
pub enum ConfirmAction {
    Drop { agent_id: AgentId, recursive: bool },
    /// The kill switch (`Q`) — `agent_count` is captured at confirm-open time
    /// purely for the prompt text; the actual drop-everything happens
    /// server-side (`Request::Shutdown`), not by iterating this count.
    Shutdown { agent_count: usize },
}

/// Active while awaiting y/n confirmation for `d`/`D`/`Q`.
pub struct ConfirmState {
    pub action: ConfirmAction,
}

/// Which half of the tree|pane split receives keyboard input.
/// `Ctrl-l` (or `Enter`/`o`) on a live agent moves `Tree -> Pane`; `Ctrl-h` is
/// the only key `Pane` intercepts, moving back to `Tree` — everything else
/// forwards to the agent's PTY untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Pane,
}

/// `--mock` only ever runs this path — a real, fully in-process registry +
/// session manager (Phase 1-7 architecture, unchanged by the daemon split).
pub struct MockCtx(pub Arc<AppCtx>);

/// Real mode's view of the daemon it has attached to. `tree` is a client-side
/// mirror kept in sync by attach events; cursor/expand state (owned by `tree`
/// itself, per `agent::tree::AgentTree`) is never touched by those events, so
/// it stays exactly as pure client-side state as AGENTS.md requires.
pub struct DaemonState {
    socket: PathBuf,
    tree: AgentTree,
    watched: Option<AgentId>,
    grid: Option<(AgentId, GridSnapshot)>,
    write_half: UnixStream,
    events: mpsc::Receiver<AttachEvent>,
    /// Set once an `AttachEvent::Shutdown` arrives or the connection drops —
    /// `tui::run_app` treats this the same as a quit request.
    pub disconnected: bool,
}

pub enum Backend {
    Mock(MockCtx),
    Daemon(DaemonState),
}

pub struct App {
    pub backend: Backend,
    pub tick: u64,
    pub input: Option<InputState>,
    pub confirm: Option<ConfirmState>,
    pub status_message: Option<String>,
    pub focus: Focus,
    /// `?` popup (PHASE5B.md) — any key closes it; doesn't interact with
    /// `input`/`confirm` at all, so it can overlay either.
    pub show_help: bool,
}

impl App {
    pub fn new(ctx: Arc<AppCtx>) -> Self {
        Self::from_backend(Backend::Mock(MockCtx(ctx)))
    }

    pub fn new_daemon(state: DaemonState) -> Self {
        Self::from_backend(Backend::Daemon(state))
    }

    fn from_backend(backend: Backend) -> Self {
        Self {
            backend,
            tick: 0,
            input: None,
            confirm: None,
            status_message: None,
            focus: Focus::Tree,
            show_help: false,
        }
    }

    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
        self.drain_events();
    }

    /// Applies every attach event received since the last tick — the real
    /// (non-mock) mode's only path for external state to reach the UI, per
    /// AGENTS.md's "status is push, not pull" now extended past the daemon
    /// boundary to the TUI itself. No-op for mock mode.
    fn drain_events(&mut self) {
        let Backend::Daemon(state) = &mut self.backend else { return };
        loop {
            match state.events.try_recv() {
                Ok(event) => apply_event(state, event),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    state.disconnected = true;
                    break;
                }
            }
        }
    }

    pub fn with_tree<R>(&self, f: impl FnOnce(&AgentTree) -> R) -> R {
        match &self.backend {
            Backend::Mock(ctx) => ctx.0.registry.with_tree(f),
            Backend::Daemon(state) => f(&state.tree),
        }
    }

    pub fn with_tree_mut<R>(&mut self, f: impl FnOnce(&mut AgentTree) -> R) -> R {
        match &mut self.backend {
            Backend::Mock(ctx) => ctx.0.registry.with_tree_mut(f),
            Backend::Daemon(state) => f(&mut state.tree),
        }
    }

    /// Whether `id`'s session still has a live process behind it. Mock mode
    /// asks `SessionManager` directly; daemon mode has no such handle, so it
    /// leans on an invariant of the status model instead (AGENTS.md
    /// Cleanup): `Done`/`Error` are set *only* when the PTY has already
    /// exited (an explicit push or the exit-code sweep), and by every other
    /// status the process is still running — so "not Done, not Error" is an
    /// honest stand-in for "alive", not a guess.
    pub fn is_alive(&self, id: &AgentId) -> bool {
        match &self.backend {
            Backend::Mock(ctx) => ctx.0.sessions.is_alive(id),
            Backend::Daemon(state) => state
                .tree
                .find(id)
                .is_some_and(|n| !matches!(n.status, AgentStatus::Done | AgentStatus::Error)),
        }
    }

    /// The `TermMode` bits `session::keys::encode_key`/`encode_paste` need to
    /// encode a keystroke/paste correctly (application-cursor arrows,
    /// bracketed paste). Mock mode reads its local `Term` directly; daemon
    /// mode rebuilds it from the last received `GridSnapshot`'s flags.
    pub fn term_mode(&self, id: &AgentId) -> alacritty_terminal::term::TermMode {
        match &self.backend {
            Backend::Mock(ctx) => {
                ctx.0.sessions.with_term(id, |term| *term.mode()).unwrap_or_default()
            }
            Backend::Daemon(state) => state
                .grid
                .as_ref()
                .filter(|(gid, _)| gid == id)
                .map(|(_, grid)| {
                    crate::session::keys::term_mode_from_flags(
                        grid.app_cursor_mode,
                        grid.bracketed_paste_mode,
                    )
                })
                .unwrap_or_default(),
        }
    }

    /// Forwards `bytes` to `id`'s PTY — the input path for jump-in keystrokes.
    pub fn write_input(&mut self, id: &AgentId, bytes: Vec<u8>) {
        match &mut self.backend {
            Backend::Mock(ctx) => ctx.0.sessions.write(id, bytes),
            Backend::Daemon(state) => {
                // Every byte this app ever writes to a PTY originates as
                // either an ASCII control byte or real UTF-8 text
                // (`session::keys::encode_key`/`encode_paste`) — lossy only
                // guards against a future encoding regression, it never
                // fires in practice.
                let data = String::from_utf8_lossy(&bytes).into_owned();
                state.send(&Request::Write { agent_id: id.clone(), data });
            }
        }
    }

    /// Resizes every agent's PTY to the one shared rect (AGENTS.md: all
    /// agents share a single size).
    pub fn resize(&mut self, cols: usize, lines: usize) {
        match &mut self.backend {
            Backend::Mock(ctx) => ctx.0.sessions.resize_all(cols, lines),
            Backend::Daemon(state) => {
                state.send(&Request::Resize { cols: cols as u16, lines: lines as u16 })
            }
        }
    }

    /// Starts (or switches) streaming `id`'s terminal output on this attach
    /// connection. No-op for mock mode — `render_term_pane` reads straight
    /// from `SessionManager` there, no watch concept needed.
    pub fn watch(&mut self, id: &AgentId) {
        if let Backend::Daemon(state) = &mut self.backend {
            if state.watched.as_ref() != Some(id) {
                state.watched = Some(id.clone());
                state.grid = None;
                state.send(&Request::Watch { agent_id: id.clone() });
            }
        }
    }

    pub fn unwatch(&mut self) {
        if let Backend::Daemon(state) = &mut self.backend {
            if state.watched.take().is_some() {
                state.grid = None;
                state.send(&Request::Unwatch);
            }
        }
    }

    /// The watched agent's most recently received rendered grid, for
    /// `ui::term_pane` to paint in daemon mode. `None` in mock mode (or
    /// before the first grid arrives).
    pub fn watched_grid(&self, id: &AgentId) -> Option<&GridSnapshot> {
        match &self.backend {
            Backend::Mock(_) => None,
            Backend::Daemon(state) => {
                state.grid.as_ref().filter(|(gid, _)| gid == id).map(|(_, grid)| grid)
            }
        }
    }

    /// Scrolls `id`'s terminal history — positive `delta` moves up (further
    /// into scrollback), negative moves down (toward live). Mock mode calls
    /// `SessionManager` directly; daemon mode sends `Request::Scroll`, which
    /// only ever affects whichever agent this connection is currently
    /// watching (server-side — `id` is not part of the wire request, see
    /// `Request::Scroll`'s doc comment) — callers are expected to only invoke
    /// this for the currently selected/watched agent.
    pub fn scroll(&mut self, id: &AgentId, delta: i32) {
        match &mut self.backend {
            Backend::Mock(ctx) => ctx.0.sessions.scroll_display(id, delta),
            Backend::Daemon(state) => state.send(&Request::Scroll { delta }),
        }
    }

    /// Jumps `id`'s terminal back to the live bottom (`G`). Same per-backend
    /// split as `scroll`.
    pub fn scroll_to_bottom(&mut self, id: &AgentId) {
        match &mut self.backend {
            Backend::Mock(ctx) => ctx.0.sessions.scroll_to_bottom(id),
            Backend::Daemon(state) => state.send(&Request::ScrollToBottom),
        }
    }

    /// Sends a one-shot request (`Start`/`Spawn`/`Drop`/…) and waits for its
    /// response. Mock mode dispatches in-process; daemon mode opens a fresh
    /// one-shot connection — deliberately *not* the persistent attach
    /// connection, which only ever speaks `AttachEvent` outward and
    /// `Watch`/`Unwatch`/`Write`/`Resize` inward.
    pub fn dispatch(&self, req: Request) -> Response {
        match &self.backend {
            Backend::Mock(ctx) => crate::ipc::handlers::dispatch(&ctx.0, req),
            Backend::Daemon(state) => crate::ipc::client::send(&state.socket, &req)
                .unwrap_or_else(|e| Response::err(e.to_string())),
        }
    }
}

/// Applies one attach event to the client-side mirror. Registrations/removals/
/// status changes update `tree`; `Output` updates the cached grid only when it
/// matches the currently watched agent (a stale reply from just before an
/// `Unwatch`/`Watch` switch is simply dropped).
fn apply_event(state: &mut DaemonState, event: AttachEvent) {
    match event {
        AttachEvent::Snapshot { agents } => {
            // A `Snapshot` isn't only the initial one anymore — a lagged
            // registry-event receiver now triggers a resync one too (a real
            // "agent is not running" bug traced back to a permanently-stale
            // client-side status after a dropped StatusChanged with no
            // Snapshot to correct it), so this must behave well mid-session,
            // not just at connect: preserve the current selection across the
            // rebuild rather than silently resetting the cursor to the top.
            let selected_id = state.tree.selected().map(|n| n.id);
            state.tree = AgentTree::new();
            for agent in agents {
                insert_dto(&mut state.tree, agent);
            }
            if let Some(id) = selected_id {
                if let Some(pos) = state.tree.flatten().iter().position(|n| n.id == id) {
                    state.tree.cursor = pos;
                }
            }
        }
        AttachEvent::AgentRegistered { agent } => insert_dto(&mut state.tree, agent),
        AttachEvent::AgentRemoved { agent_id } => {
            state.tree.remove(&agent_id);
        }
        AttachEvent::StatusChanged { agent_id, status, context_pct, message: _ } => {
            if let Some(node) = state.tree.find_mut(&agent_id) {
                // Same "compare before overwrite" rule as the registry
                // itself (ATTENTION.md) — a repeated same-status push must
                // not reset the client's own clock either. The event carries
                // no status_secs of its own (see `AttachEvent::StatusChanged`
                // doc comment); the client's own `Instant::now()` at the
                // moment of an actual change is accurate enough.
                if node.status != status {
                    node.status_since = std::time::Instant::now();
                }
                node.status = status;
                // The server already merged this into the node's current
                // value before broadcasting (see `AgentRegistry::set_status`)
                // — this is the definitive value, not a delta to fold in.
                node.context_pct = context_pct;
            }
        }
        AttachEvent::Output { agent_id, grid } => {
            if state.watched.as_ref() == Some(&agent_id) {
                state.grid = Some((agent_id, grid));
            }
        }
        AttachEvent::Shutdown => state.disconnected = true,
    }
}

/// Converts a wire `AgentDto` into a tree node and inserts it — as a root if
/// it has no parent, as a child of `parent_id` otherwise. A child arriving
/// before its parent (e.g. right after a `Lagged` broadcast gap) is silently
/// dropped rather than panicking; the next full `Snapshot` re-syncs it.
fn insert_dto(tree: &mut AgentTree, dto: AgentDto) {
    // Seeds the client's own clock reference from the daemon's reported age
    // — `checked_sub` rather than bare `-` since `Instant` has no fixed
    // epoch to safely subtract an unbounded duration from; falling back to
    // "now" (age 0) is a harmless display-only discrepancy, not a panic.
    let status_since = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(dto.status_secs))
        .unwrap_or_else(std::time::Instant::now);
    let node = AgentNode {
        id: dto.id,
        name: dto.name,
        status: dto.status,
        role: dto.role,
        repo: dto.repo,
        branch: dto.branch,
        adapter: dto.adapter,
        cwd: dto.cwd,
        context_pct: dto.context_pct,
        children: Vec::new(),
        expanded: true,
        status_since,
    };
    match dto.parent_id {
        None => tree.add_root(node),
        Some(parent_id) => {
            tree.insert_child(&parent_id, node);
        }
    }
}

impl DaemonState {
    /// Connects to `socket`, upgrades via `Request::Attach`, and blocks
    /// briefly for the initial `Snapshot` so the TUI never renders an empty
    /// tree for agents that already existed before this attach.
    pub fn connect(socket: PathBuf) -> Result<Self> {
        let mut write_half = UnixStream::connect(&socket)
            .with_context(|| format!("failed to connect to {}", socket.display()))?;
        let read_half = write_half
            .try_clone()
            .context("failed to clone the attach connection for its reader thread")?;

        let attach_line = serde_json::to_string(&Request::Attach)? + "\n";
        write_half.write_all(attach_line.as_bytes())?;
        write_half.flush()?;

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(read_half);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if let Ok(event) = serde_json::from_str::<AttachEvent>(line.trim()) {
                            if tx.send(event).is_err() {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let mut state = Self {
            socket,
            tree: AgentTree::new(),
            watched: None,
            grid: None,
            write_half,
            events: rx,
            disconnected: false,
        };
        match state.events.recv_timeout(Duration::from_secs(5)) {
            Ok(event) => apply_event(&mut state, event),
            Err(_) => anyhow::bail!("daemon at {} did not send an initial snapshot", state.socket.display()),
        }
        Ok(state)
    }

    fn send(&mut self, req: &Request) {
        let Ok(json) = serde_json::to_string(req) else { return };
        let mut line = json.into_bytes();
        line.push(b'\n');
        let _ = self.write_half.write_all(&line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentRegistry, AgentRole};
    use crate::git::GitClient;
    use crate::session::SessionManager;
    use std::path::PathBuf;

    fn test_app() -> App {
        let ctx = Arc::new(AppCtx {
            registry: Arc::new(AgentRegistry::new()),
            sessions: Arc::new(SessionManager::dry_run()),
            socket: PathBuf::from("/tmp/overseer-test.sock"),
            git: Arc::new(GitClient::new()),
            config: Arc::new(crate::config::Config::default()),
            watch_sessions: false,
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        });
        App::new(ctx)
    }

    #[test]
    fn new_app_starts_with_no_input_or_confirm_pending() {
        let app = test_app();
        assert!(app.input.is_none());
        assert!(app.confirm.is_none());
    }

    #[test]
    fn new_app_starts_focused_on_the_tree() {
        let app = test_app();
        assert_eq!(app.focus, Focus::Tree);
    }

    // ── daemon-mode event application ────────────────────────────────────────

    fn dto(id: AgentId, parent_id: Option<AgentId>, status: AgentStatus) -> AgentDto {
        AgentDto {
            id,
            name: "agent".to_string(),
            status,
            role: if parent_id.is_none() { AgentRole::Root } else { AgentRole::Child },
            parent_id,
            adapter: "claude".to_string(),
            repo: "overseer".to_string(),
            branch: "main".to_string(),
            cwd: PathBuf::from("/tmp"),
            context_pct: None,
            status_secs: 0,
        }
    }

    fn empty_daemon_state() -> DaemonState {
        // Never actually attached — only `apply_event`/tree/watch state is
        // exercised, so the socket/write_half/events channel are unused.
        let (_tx, rx) = mpsc::channel();
        DaemonState {
            socket: PathBuf::from("/tmp/unused.sock"),
            tree: AgentTree::new(),
            watched: None,
            grid: None,
            write_half: UnixStream::pair().unwrap().0,
            events: rx,
            disconnected: false,
        }
    }

    #[test]
    fn snapshot_populates_roots_and_children_in_order() {
        let mut state = empty_daemon_state();
        let root_id = AgentId::new();
        let child_id = AgentId::new();
        apply_event(&mut state, AttachEvent::Snapshot {
            agents: vec![
                dto(root_id.clone(), None, AgentStatus::Idle),
                dto(child_id.clone(), Some(root_id.clone()), AgentStatus::Running),
            ],
        });
        assert_eq!(state.tree.flatten().len(), 2);
        assert!(state.tree.find(&root_id).is_some());
        assert!(state.tree.find(&child_id).is_some());
    }

    #[test]
    fn a_second_snapshot_preserves_the_current_selection() {
        // A lagged registry-event receiver now triggers a resync Snapshot
        // mid-session (ipc::server), not just at initial connect — this must
        // not silently reset the user's cursor back to the top of the tree.
        // `ctx.registry.snapshot()` always walks parent-before-child (it
        // recurses the real tree top-down), so the realistic case a resync
        // must handle isn't "the same agents reordered" but "the selected
        // agent's *flat index* shifted because something else in the tree
        // changed in the interim" (here: a second root registered ahead of
        // it in snapshot order).
        let mut state = empty_daemon_state();
        let root_id = AgentId::new();
        let child_id = AgentId::new();
        apply_event(&mut state, AttachEvent::Snapshot {
            agents: vec![
                dto(root_id.clone(), None, AgentStatus::Idle),
                dto(child_id.clone(), Some(root_id.clone()), AgentStatus::Running),
            ],
        });
        state.tree.cursor = 1; // select the child
        assert_eq!(state.tree.selected().unwrap().id, child_id);

        // Resync: a new root now sorts ahead of the original root, pushing
        // the child's flat index from 1 to 2.
        let new_root_id = AgentId::new();
        apply_event(&mut state, AttachEvent::Snapshot {
            agents: vec![
                dto(new_root_id, None, AgentStatus::Idle),
                dto(root_id.clone(), None, AgentStatus::Idle),
                dto(child_id.clone(), Some(root_id.clone()), AgentStatus::Running),
            ],
        });

        assert_eq!(state.tree.selected().unwrap().id, child_id, "selection must follow the same agent");
        assert_eq!(state.tree.cursor, 2, "cursor should track the child's new flat index");
    }

    #[test]
    fn a_second_snapshot_falls_back_to_the_top_if_the_selected_agent_is_gone() {
        let mut state = empty_daemon_state();
        let root_id = AgentId::new();
        let child_id = AgentId::new();
        apply_event(&mut state, AttachEvent::Snapshot {
            agents: vec![
                dto(root_id.clone(), None, AgentStatus::Idle),
                dto(child_id.clone(), Some(root_id.clone()), AgentStatus::Running),
            ],
        });
        state.tree.cursor = 1; // select the child

        // Resync without the child (dropped in the interim).
        apply_event(&mut state, AttachEvent::Snapshot { agents: vec![dto(root_id.clone(), None, AgentStatus::Idle)] });

        assert_eq!(state.tree.selected().unwrap().id, root_id);
    }

    #[test]
    fn agent_registered_root_then_child_builds_hierarchy() {
        let mut state = empty_daemon_state();
        let root_id = AgentId::new();
        apply_event(&mut state, AttachEvent::AgentRegistered { agent: dto(root_id.clone(), None, AgentStatus::Idle) });
        let child_id = AgentId::new();
        apply_event(&mut state, AttachEvent::AgentRegistered {
            agent: dto(child_id.clone(), Some(root_id.clone()), AgentStatus::Running),
        });
        assert_eq!(state.tree.roots.len(), 1);
        assert_eq!(state.tree.roots[0].children.len(), 1);
        assert_eq!(state.tree.roots[0].children[0].id, child_id);
    }

    #[test]
    fn agent_removed_drops_the_node() {
        let mut state = empty_daemon_state();
        let root_id = AgentId::new();
        apply_event(&mut state, AttachEvent::AgentRegistered { agent: dto(root_id.clone(), None, AgentStatus::Idle) });
        apply_event(&mut state, AttachEvent::AgentRemoved { agent_id: root_id.clone() });
        assert!(state.tree.find(&root_id).is_none());
    }

    #[test]
    fn status_changed_overwrites_status_and_context_pct() {
        let mut state = empty_daemon_state();
        let root_id = AgentId::new();
        apply_event(&mut state, AttachEvent::AgentRegistered { agent: dto(root_id.clone(), None, AgentStatus::Idle) });
        apply_event(&mut state, AttachEvent::StatusChanged {
            agent_id: root_id.clone(),
            status: AgentStatus::Blocked,
            message: None,
            context_pct: Some(42),
        });
        let node = state.tree.find(&root_id).unwrap();
        assert_eq!(node.status, AgentStatus::Blocked);
        assert_eq!(node.context_pct, Some(42));
    }

    #[test]
    fn status_changed_same_status_keeps_client_side_status_since() {
        let mut state = empty_daemon_state();
        let root_id = AgentId::new();
        apply_event(&mut state, AttachEvent::AgentRegistered { agent: dto(root_id.clone(), None, AgentStatus::Running) });
        let before = state.tree.find(&root_id).unwrap().status_since;

        apply_event(&mut state, AttachEvent::StatusChanged {
            agent_id: root_id.clone(),
            status: AgentStatus::Running,
            message: None,
            context_pct: None,
        });

        let after = state.tree.find(&root_id).unwrap().status_since;
        assert_eq!(before, after, "a repeated same-status event must not reset the client's clock");
    }

    #[test]
    fn status_changed_actual_change_resets_client_side_status_since() {
        let mut state = empty_daemon_state();
        let root_id = AgentId::new();
        apply_event(&mut state, AttachEvent::AgentRegistered { agent: dto(root_id.clone(), None, AgentStatus::Running) });
        let before = state.tree.find(&root_id).unwrap().status_since;

        std::thread::sleep(std::time::Duration::from_millis(5));
        apply_event(&mut state, AttachEvent::StatusChanged {
            agent_id: root_id.clone(),
            status: AgentStatus::Blocked,
            message: None,
            context_pct: None,
        });

        let after = state.tree.find(&root_id).unwrap().status_since;
        assert!(after > before, "an actual status change must reset the client's clock");
    }

    #[test]
    fn insert_dto_seeds_status_since_from_status_secs() {
        let mut state = empty_daemon_state();
        let root_id = AgentId::new();
        let mut agent = dto(root_id.clone(), None, AgentStatus::Idle);
        agent.status_secs = 120;
        let before_insert = std::time::Instant::now();
        apply_event(&mut state, AttachEvent::AgentRegistered { agent });

        let status_since = state.tree.find(&root_id).unwrap().status_since;
        // Seeded ~120s in the past relative to "now", well before this test's
        // own start — a generous bound avoids flaking on slow CI machines.
        assert!(status_since <= before_insert, "a reported age must be seeded into the past, not the future");
    }

    #[test]
    fn status_changed_with_no_context_pct_clears_it() {
        // The server always broadcasts the node's definitive current value —
        // `None` here means the node genuinely has no known context_pct, not
        // "leave whatever the client already had".
        let mut state = empty_daemon_state();
        let root_id = AgentId::new();
        apply_event(&mut state, AttachEvent::AgentRegistered { agent: dto(root_id.clone(), None, AgentStatus::Idle) });
        apply_event(&mut state, AttachEvent::StatusChanged {
            agent_id: root_id.clone(),
            status: AgentStatus::Running,
            message: None,
            context_pct: Some(10),
        });
        apply_event(&mut state, AttachEvent::StatusChanged {
            agent_id: root_id.clone(),
            status: AgentStatus::Idle,
            message: None,
            context_pct: None,
        });
        assert_eq!(state.tree.find(&root_id).unwrap().context_pct, None);
    }

    #[test]
    fn child_registered_before_its_parent_is_dropped_not_panicking() {
        let mut state = empty_daemon_state();
        let orphan_child = dto(AgentId::new(), Some(AgentId::new()), AgentStatus::Running);
        apply_event(&mut state, AttachEvent::AgentRegistered { agent: orphan_child });
        assert!(state.tree.roots.is_empty());
    }

    #[test]
    fn output_only_updates_grid_for_the_currently_watched_agent() {
        let mut state = empty_daemon_state();
        let watched_id = AgentId::new();
        state.watched = Some(watched_id.clone());
        let grid = GridSnapshot {
            cols: 1,
            lines: 1,
            cells: vec![None],
            cursor: None,
            app_cursor_mode: false,
            bracketed_paste_mode: false,
            display_offset: 0,
        };

        // A stale reply for a different (previously watched) agent must not
        // clobber the current watch.
        apply_event(&mut state, AttachEvent::Output { agent_id: AgentId::new(), grid: grid.clone() });
        assert!(state.grid.is_none());

        apply_event(&mut state, AttachEvent::Output { agent_id: watched_id.clone(), grid });
        assert!(state.grid.is_some());
    }

    #[test]
    fn shutdown_event_sets_disconnected() {
        let mut state = empty_daemon_state();
        apply_event(&mut state, AttachEvent::Shutdown);
        assert!(state.disconnected);
    }

    // ── App is_alive across backends ─────────────────────────────────────────

    #[test]
    fn daemon_mode_is_alive_is_false_for_done_and_error_true_otherwise() {
        let mut state = empty_daemon_state();
        for (status, expected) in [
            (AgentStatus::Spawning, true),
            (AgentStatus::Running, true),
            (AgentStatus::Blocked, true),
            (AgentStatus::Idle, true),
            (AgentStatus::Done, false),
            (AgentStatus::Error, false),
        ] {
            let id = AgentId::new();
            apply_event(&mut state, AttachEvent::AgentRegistered { agent: dto(id.clone(), None, status) });
            let app = App::new_daemon_state_for_test(state);
            assert_eq!(app.is_alive(&id), expected);
            state = app.into_daemon_state_for_test();
        }
    }

    #[test]
    fn daemon_mode_is_alive_is_false_for_unknown_id() {
        let state = empty_daemon_state();
        let app = App::new_daemon_state_for_test(state);
        assert!(!app.is_alive(&AgentId::new()));
    }

    #[test]
    fn mock_mode_dispatch_registers_a_root_in_process() {
        let app = test_app();
        let resp = app.dispatch(Request::Start { cwd: Some(PathBuf::from("/tmp")) });
        assert!(resp.ok, "dispatch failed: {:?}", resp.error);
    }

    // ── App::scroll / scroll_to_bottom (mock mode) ───────────────────────────

    #[test]
    fn mock_mode_scroll_on_unknown_id_does_not_panic() {
        let mut app = test_app();
        app.scroll(&AgentId::new(), 5);
        app.scroll_to_bottom(&AgentId::new());
    }

    #[test]
    fn mock_mode_scroll_and_scroll_to_bottom_reach_the_real_session_manager() {
        let sessions = SessionManager::new();
        let id = AgentId::new();
        // More lines than the default 24-line grid so there's real scrollback
        // history to move into — printing fewer would leave history_size() at
        // 0 and clamp any Scroll::Delta straight back to 0.
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "i=0; while [ $i -lt 60 ]; do echo line$i; i=$((i+1)); done; sleep 60"]);
        sessions.launch(id.clone(), &PathBuf::from("/tmp"), &cmd, &std::collections::HashMap::new()).unwrap();

        let became_dirty = (0..50).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            sessions.take_dirty(&id)
        });
        assert!(became_dirty, "session must produce output before there's scrollback to move into");

        let ctx = Arc::new(AppCtx {
            registry: Arc::new(AgentRegistry::new()),
            sessions: Arc::new(sessions),
            socket: PathBuf::from("/tmp/overseer-scroll-test.sock"),
            git: Arc::new(GitClient::new()),
            config: Arc::new(crate::config::Config::default()),
            watch_sessions: false,
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        });
        let mut app = App::new(ctx.clone());

        app.scroll(&id, 5);
        assert_eq!(ctx.sessions.display_offset(&id), 5);
        app.scroll_to_bottom(&id);
        assert_eq!(ctx.sessions.display_offset(&id), 0);

        ctx.sessions.kill(&id);
    }

    // A tiny escape hatch so the daemon-mode tests above can build an `App`
    // around a hand-built `DaemonState` without going through a real socket —
    // only used by this test module.
    impl App {
        fn new_daemon_state_for_test(state: DaemonState) -> Self {
            App::new_daemon(state)
        }

        fn into_daemon_state_for_test(self) -> DaemonState {
            match self.backend {
                Backend::Daemon(state) => state,
                Backend::Mock(_) => unreachable!("test-only helper never used with mock mode"),
            }
        }
    }
}
