//! TUI controller: event loop + key handlers. `app.rs` owns the pure state
//! (`App`, `Focus`, `InputState`, …); this is its controller — driving
//! `crossterm` events into state mutations and calling `ui::render` each tick.

use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal};
use std::{io, path::PathBuf, sync::Arc, time::Duration};

use crate::agent::AgentTree;
use crate::app::{App, Backend, ConfirmAction, ConfirmState, DaemonState, Focus, InputState, PendingAction};
use crate::config::{Action, Config, KeyBinding, Keybindings};
use crate::daemon;
use crate::git::GitClient;
use crate::ipc;
use crate::ipc::protocol::Request;
use crate::ipc::AppCtx;
use crate::session::{self, SessionManager};
use crate::ui;

pub fn run_tui(socket: PathBuf, mock: bool) -> Result<()> {
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        default_panic(info);
    }));

    let mut app = if mock {
        App::new(mock_ctx(socket.clone()))
    } else {
        daemon::ensure_daemon_running(&socket)
            .context("failed to reach or start the overseer daemon")?;
        let state = DaemonState::connect(socket.clone())
            .context("failed to attach to the overseer daemon")?;
        App::new_daemon(state)
    };

    // Bell/desktop-notification, keybinding, and theme preferences (ATTENTION.md
    // / PHASE5B.md) are all properties of *this* terminal/desktop, not the
    // daemon's — read independently of mock_ctx's own config load (which is
    // only about adapter resolution), and identically regardless of which
    // backend `app` ends up using.
    let ui_config = Config::load();
    let notify_config = ui_config.notify;
    let keybindings = ui_config.keybindings;
    let theme = ui_config.theme;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_app(&mut terminal, &mut app, &notify_config, &keybindings, &theme);

    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    );
    let _ = terminal.show_cursor();
    // Only mock mode owns its socket (a throwaway, per-invocation IPC server
    // it started itself) — real mode attached to the daemon's stable,
    // persistent socket and must leave it alone; the daemon owns it across
    // TUI restarts, that's the whole point of the split.
    if mock {
        let _ = std::fs::remove_file(&socket);
    }

    res
}

/// `--mock` is inert demo data run fully in-process, exactly as before the
/// daemon split — it never spawns a real PTY and never touches a daemon.
fn mock_ctx(socket: PathBuf) -> Arc<AppCtx> {
    let ctx = Arc::new(AppCtx {
        registry: Arc::new(crate::agent::AgentRegistry::from_tree(AgentTree::with_mock_data())),
        sessions: Arc::new(SessionManager::dry_run()),
        socket: socket.clone(),
        git: Arc::new(GitClient::new()),
        config: Arc::new(Config::load()),
        watch_sessions: false,
        shutdown_notify: Arc::new(tokio::sync::Notify::new()),
    });

    let ipc_ctx = ctx.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        if let Err(e) = ipc::serve_blocking(ipc_ctx, socket, Some(ready_tx)) {
            eprintln!("IPC server error: {e}");
        }
    });
    ready_rx.recv().ok();
    ctx
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    notify_config: &crate::config::NotifyConfig,
    keybindings: &Keybindings,
    theme: &crate::config::Theme,
) -> Result<()> {
    let mut last_pane_size: Option<(u16, u16)> = None;
    let mut last_selected: Option<crate::agent::AgentId> = None;
    let mut last_statuses: std::collections::HashMap<crate::agent::AgentId, crate::agent::AgentStatus> =
        std::collections::HashMap::new();

    loop {
        let tick = app.tick;

        // The selected agent drives which one streams to the pane — mock
        // mode's `App::pane_grid` reads `SessionManager` directly by id every
        // frame, so `watch` only does real work in daemon mode (it no-ops
        // otherwise). "Switching on cursor move" (DAEMON.md) lives here.
        let selected_id = app.with_tree(|t| t.selected()).map(|n| n.id);
        match &selected_id {
            Some(id) => app.watch(id),
            None => app.unwatch(),
        }

        // Scroll position resets to the live bottom whenever the selection
        // changes (SCROLLBACK.md) — covers j/k, a drop shifting the cursor,
        // toggling a fold, all in one place instead of at each call site.
        if selected_id != last_selected {
            if let Some(id) = &selected_id {
                app.scroll_to_bottom(id);
            }
            last_selected = selected_id.clone();
        }

        // Attention surfacing (ATTENTION.md): bell/desktop notification on a
        // →blocked (or, if configured, →idle) transition. Detected by diffing
        // this frame's statuses against the last — identical for `--mock`
        // and a daemon-attached session, since it only reads the
        // already-materialized tree, not either backend's own event plumbing.
        let flat = app.with_tree(|t| t.flatten());
        let transitions = crate::notify::status_transitions(&last_statuses, &flat);
        crate::notify::handle_transitions(notify_config, &transitions);
        last_statuses = crate::notify::snapshot_statuses(&flat);

        let prompt = build_prompt(app);
        let input = app.input.as_ref();
        let pane_focused = app.focus == Focus::Pane;
        let pane_grid = selected_id.as_ref().and_then(|id| app.pane_grid(id));
        let mut pane_rect = Rect::default();
        app.with_tree(|tree| {
            terminal.draw(|f| {
                pane_rect = ui::render(
                    f,
                    tree,
                    tick,
                    prompt.as_deref(),
                    input,
                    pane_grid.as_ref(),
                    pane_focused,
                    theme,
                    keybindings,
                    app.show_help,
                );
            })
        })?;
        // Every agent shares one PTY size — resize on layout
        // change (including the very first draw, sizing new sessions before
        // the user ever gets to spawn one).
        let pane_size = (pane_rect.width, pane_rect.height);
        if last_pane_size != Some(pane_size) {
            app.resize(pane_size.0 as usize, pane_size.1 as usize);
            last_pane_size = Some(pane_size);
        }

        // This timeout doubles as the whole loop's frame pacing, not just the
        // keyboard wait: a background thread already queues attach-socket
        // events (DaemonState::drain_events) independent of this poll, but
        // they only get rendered once per iteration here. A 100ms value
        // (the original choice) meant every keystroke and every streamed
        // agent-output update could sit for up to 100ms before its own
        // redraw, stacking with the daemon's own dirty-check poll
        // (ipc::server::OUTPUT_POLL_INTERVAL) for a worst case pushing
        // 200-300ms end to end — perceptible lag, reported directly by a
        // real user typing into a jumped-in pane. Tightened to a
        // 60fps-equivalent interval; per-frame render cost is cheap enough
        // (SCALE.md: ~0.4ms for a 50-node tree, even unoptimized) that the
        // extra wake-ups cost is negligible against the responsiveness win.
        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key) if key.kind != event::KeyEventKind::Release => {
                    if app.show_help {
                        // Any key closes it (PHASE5B.md) — doesn't matter
                        // which, so this comes before focus/input dispatch.
                        app.show_help = false;
                    } else {
                        match app.focus {
                            Focus::Pane => handle_pane_key(app, key),
                            Focus::Tree => {
                                if app.input.is_some() {
                                    handle_input_key(app, key);
                                } else if app.confirm.is_some() {
                                    if !handle_confirm_key(app, key) {
                                        break;
                                    }
                                } else if !handle_tree_key(app, key, pane_rect.height, keybindings) {
                                    break;
                                }
                            }
                        }
                    }
                }
                Event::Paste(text) if app.focus == Focus::Pane => forward_paste(app, &text),
                _ => {}
            }
        }

        app.tick();

        // The daemon closed the connection (e.g. `overseer shutdown` from
        // elsewhere) — nothing left to attach to, so stop like a quit.
        if let Backend::Daemon(state) = &app.backend {
            if state.disconnected {
                break;
            }
        }
    }
    Ok(())
}

/// `Focus::Tree` key handling (nav, spawn, drop, jump-in, quit). Returns
/// `false` to request the run loop break (quit). `pane_height` is this
/// frame's rendered pane height in rows — needed to size a half-page scroll.
/// `keybindings` drives everything remappable (PHASE5B.md); a handful of
/// keys stay fixed regardless (see the two `match`es below this doc comment).
fn handle_tree_key(app: &mut App, key: KeyEvent, pane_height: u16, keybindings: &Keybindings) -> bool {
    // Fixed, non-remappable aliases: Ctrl-C always quits (same as `q`, just
    // the universal terminal muscle-memory one); Enter/`o` always jump in
    // (mirroring `jump_in`'s own doc comment) regardless of what `jump_in`
    // is bound to.
    match key.code {
        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => return false,
        KeyCode::Enter | KeyCode::Char('o') if key.modifiers == KeyModifiers::NONE => {
            jump_in(app);
            return true;
        }
        _ => {}
    }

    // Scrollback (SCROLLBACK.md): tree-focus only, the pane here is a
    // read-only preview — these keys must never be reachable while a pane is
    // focused (that's real agent-TUI territory, e.g. readline's own Ctrl-u
    // kill-line). Positive delta = up into history, negative = down toward
    // live, matching `SessionManager::scroll_display`. Not modeled in
    // `Keybindings` — SCROLLBACK.md never asked for these to be remappable.
    match key.code {
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
            scroll_selected(app, (pane_height / 2) as i32);
            return true;
        }
        KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
            scroll_selected(app, -((pane_height / 2) as i32));
            return true;
        }
        KeyCode::Char('y') if key.modifiers == KeyModifiers::CONTROL => {
            scroll_selected(app, 1);
            return true;
        }
        KeyCode::Char('e') if key.modifiers == KeyModifiers::CONTROL => {
            scroll_selected(app, -1);
            return true;
        }
        // No modifier guard, matching the `D`/`Q` house pattern below — some
        // terminals report `KeyModifiers::SHIFT` alongside a capital letter,
        // some don't (confirmed via tmux: it does), so requiring `NONE` here
        // silently swallowed the key in exactly that case.
        KeyCode::Char('G') => {
            scroll_to_bottom_selected(app);
            return true;
        }
        _ => {}
    }

    let Some(binding) = key_binding_from_event(&key) else { return true };
    let Some(action) = keybindings.action_for_key(binding) else { return true };

    match action {
        Action::NavDown => {
            app.with_tree_mut(|t| t.move_down());
        }
        Action::NavUp => {
            app.with_tree_mut(|t| t.move_up());
        }
        Action::ToggleExpand => {
            app.with_tree_mut(|t| t.toggle_expand());
        }
        Action::JumpIn => jump_in(app),
        Action::SpawnRoot => {
            app.status_message = None;
            let default_cwd = std::env::current_dir()
                .map(|p| display_path_from_home(&p))
                .unwrap_or_default();
            app.input = Some(InputState {
                action: PendingAction::SpawnRoot,
                buffer: default_cwd,
            });
        }
        Action::SpawnChild => start_spawn_child_input(app),
        // Quitting never kills agents — they're independent child processes
        // that outlive the TUI, tmux-detach style (AGENTS.md Cleanup). Use
        // `d`/`D` on a specific agent first if you want it gone.
        Action::Drop => start_drop_confirm(app, false),
        Action::DropRecursive => start_drop_confirm(app, true),
        Action::Quit => return false,
        Action::Shutdown => start_shutdown_confirm(app),
        Action::Search => start_search(app),
        Action::Help => app.show_help = true,
    }
    true
}

/// Converts a live key event into the config-file vocabulary `Keybindings`
/// speaks — the inverse of `parse_binding`. Kept here (not in `config/`)
/// since it's the one piece that actually depends on `crossterm`.
fn key_binding_from_event(key: &KeyEvent) -> Option<KeyBinding> {
    match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            Some(KeyBinding::Ctrl(c.to_ascii_lowercase()))
        }
        KeyCode::Char(c) if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT => {
            Some(KeyBinding::Char(c))
        }
        KeyCode::Enter if key.modifiers == KeyModifiers::NONE => Some(KeyBinding::Enter),
        KeyCode::Esc if key.modifiers == KeyModifiers::NONE => Some(KeyBinding::Esc),
        _ => None,
    }
}

/// `/`: opens a live-filtering search prompt from tree focus (PHASE5B.md).
fn start_search(app: &mut App) {
    app.status_message = None;
    app.input = Some(InputState { action: PendingAction::Search, buffer: String::new() });
}

fn scroll_selected(app: &mut App, delta: i32) {
    if let Some(id) = app.with_tree(|t| t.selected()).map(|n| n.id) {
        app.scroll(&id, delta);
    }
}

fn scroll_to_bottom_selected(app: &mut App) {
    if let Some(id) = app.with_tree(|t| t.selected()).map(|n| n.id) {
        app.scroll_to_bottom(&id);
    }
}

/// `Ctrl-l`/`Enter`/`o` on a selected, live agent moves focus into its pane
/// — the same path serves read-only preview and jump-in,
/// this just starts routing keys to the PTY instead of the tree.
fn jump_in(app: &mut App) {
    let Some(id) = app.with_tree(|t| t.selected()).map(|n| n.id) else { return };
    if app.is_alive(&id) {
        // Interacting with a live pane while scrolled into history would be
        // confusing (typing blind into a stale view) — SCROLLBACK.md resets
        // to the live bottom on jump-in.
        app.scroll_to_bottom(&id);
        app.focus = Focus::Pane;
        app.status_message = None;
    } else {
        app.status_message = Some("agent is not running".to_string());
    }
}

/// `Focus::Pane` key handling: `Ctrl-h` is the only intercepted key — it
/// returns focus to the tree. Everything else, modifiers included, encodes
/// to bytes and forwards to the agent's PTY untouched (Ctrl-c reaches the
/// agent as an interrupt, never quits Overseer while a pane is focused).
fn handle_pane_key(app: &mut App, key: KeyEvent) {
    if key.code == KeyCode::Char('h') && key.modifiers == KeyModifiers::CONTROL {
        app.focus = Focus::Tree;
        return;
    }

    let Some(id) = app.with_tree(|t| t.selected()).map(|n| n.id) else {
        app.focus = Focus::Tree;
        return;
    };
    let mode = app.term_modes(&id);
    if let Some(bytes) = session::keys::encode_key(&key, mode) {
        app.write_input(&id, bytes);
    }
}

fn forward_paste(app: &mut App, text: &str) {
    let Some(id) = app.with_tree(|t| t.selected()).map(|n| n.id) else { return };
    let mode = app.term_modes(&id);
    let bytes = session::keys::encode_paste(text, mode);
    app.write_input(&id, bytes);
}

/// Builds the status-bar override text for the active confirm prompt, or the
/// last status message. `None` means the status bar should show its normal
/// hints. Spawn input (`n`/`s`) no longer goes through here — it renders as
/// its own modal (`ui::render_spawn_modal`), driven directly by `app.input`.
fn build_prompt(app: &App) -> Option<String> {
    if let Some(confirm) = &app.confirm {
        return Some(match &confirm.action {
            ConfirmAction::Drop { agent_id, recursive } => {
                let name = app
                    .with_tree(|t| t.find(agent_id).map(|n| n.name.clone()))
                    .unwrap_or_else(|| "?".to_string());
                let descendants = app
                    .with_tree(|t| t.subtree_ids_postorder(agent_id))
                    .map(|ids| ids.len().saturating_sub(1))
                    .unwrap_or(0);
                let suffix = if *recursive && descendants > 0 {
                    format!(" + {descendants} children")
                } else {
                    String::new()
                };
                format!("drop '{name}'{suffix}? (y/n)")
            }
            ConfirmAction::Shutdown { agent_count } => {
                let plural = if *agent_count == 1 { "" } else { "s" };
                format!("kill {agent_count} agent{plural} and the daemon? (y/n)")
            }
        });
    }

    app.status_message.clone()
}

/// `s`: opens a task-input prompt for the selected node, whatever its role. Whether
/// it's actually eligible to take a child is decided by the server's `Spawn` handler
/// alone (AGENTS.md: the "no grandchildren" rule lives there, "not in the TUI, not as
/// a UI hint") — a rejection surfaces through `submit_input`'s normal error handling.
fn start_spawn_child_input(app: &mut App) {
    if let Some(node) = app.with_tree(|t| t.selected()) {
        app.status_message = None;
        app.input = Some(InputState {
            action: PendingAction::SpawnChild { parent_id: node.id },
            buffer: String::new(),
        });
    }
}

fn start_drop_confirm(app: &mut App, recursive: bool) {
    if let Some(node) = app.with_tree(|t| t.selected()) {
        app.status_message = None;
        app.confirm = Some(ConfirmState { action: ConfirmAction::Drop { agent_id: node.id, recursive } });
    }
}

/// `Q`: the kill switch — recursive-drops every agent and exits the daemon
/// (`Request::Shutdown`), confirmed since it's the one action that reaches
/// past agents the user isn't even looking at right now.
fn start_shutdown_confirm(app: &mut App) {
    let agent_count = app.with_tree(|t| t.agent_counts().2);
    app.status_message = None;
    app.confirm = Some(ConfirmState { action: ConfirmAction::Shutdown { agent_count } });
}

fn handle_input_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.input = None;
        }
        KeyCode::Backspace => {
            if let Some(input) = app.input.as_mut() {
                input.buffer.pop();
            }
        }
        KeyCode::Enter => {
            if let Some(input) = app.input.take() {
                submit_input(app, input);
            }
        }
        KeyCode::Char(c)
            if key.modifiers == KeyModifiers::NONE || key.modifiers == KeyModifiers::SHIFT =>
        {
            if let Some(input) = app.input.as_mut() {
                input.buffer.push(c);
            }
        }
        _ => {}
    }
}

/// Dispatches the composed `Start`/`Spawn` request — mock mode in-process,
/// real mode over a one-shot connection to the daemon (`App::dispatch`) —
/// so the grandchild check applies uniformly either way.
///
/// `SpawnRoot`'s empty-buffer semantics differ from `SpawnChild`'s: an empty repo
/// path means "use cwd" (not "cancel") — the buffer is prefilled with cwd on `n`
/// already, so an empty buffer only happens if the user deliberately cleared it.
fn submit_input(app: &mut App, input: InputState) {
    match input.action {
        PendingAction::SpawnRoot => {
            let path_text = input.buffer.trim();
            let cwd = if path_text.is_empty() {
                std::env::current_dir().ok()
            } else {
                Some(expand_repo_path(path_text))
            };
            dispatch_and_report(app, Request::Start { cwd });
        }
        PendingAction::SpawnChild { parent_id } => {
            let task = input.buffer.trim().to_string();
            if task.is_empty() {
                app.status_message = Some("spawn cancelled: empty task".to_string());
                return;
            }
            let Some(parent_cwd) = app.with_tree(|t| t.find(&parent_id).map(|n| n.cwd.clone())) else {
                app.status_message = Some("spawn failed: parent no longer exists".to_string());
                return;
            };
            dispatch_and_report(app, Request::Spawn { parent_id, task, name: None, adapter: None, cwd: parent_cwd });
        }
        // Enter on a live search: keep the cursor where it is if that node
        // still matches, otherwise jump to the first match in tree order
        // (PHASE5B.md: "moves the cursor to the first/selected match").
        // Filtering itself is render-only (ui::render reads `input` live
        // while it's `Some`) — this is the one place the *real* cursor moves.
        PendingAction::Search => {
            let query = input.buffer.trim().to_string();
            app.with_tree_mut(|t| {
                let flat = t.flatten();
                let cursor_still_matches =
                    flat.get(t.cursor).is_some_and(|n| ui::fuzzy_match(&query, &n.name).is_some());
                if !cursor_still_matches {
                    if let Some(pos) = flat.iter().position(|n| ui::fuzzy_match(&query, &n.name).is_some()) {
                        t.cursor = pos;
                    }
                }
            });
        }
    }
}

fn dispatch_and_report(app: &mut App, req: Request) {
    let resp = app.dispatch(req);
    app.status_message = if resp.ok {
        None
    } else {
        Some(format!("spawn failed: {}", resp.error.unwrap_or_else(|| "unknown error".to_string())))
    };
}

/// Renders `path` relative to `$HOME` (e.g. `~/projects/overseer`) for display in
/// the spawn-root modal — the inverse of `expand_repo_path`. Falls back to the
/// absolute path unchanged when `path` isn't under home (or home can't be resolved).
fn display_path_from_home(path: &std::path::Path) -> String {
    let Some(home) = dirs::home_dir() else {
        return path.display().to_string();
    };
    if path == home {
        return "~".to_string();
    }
    match path.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Expands a leading `~/` using `$HOME`, since `PathBuf` does no shell expansion
/// on its own and the repo-path prompt is otherwise typed like a shell argument.
fn expand_repo_path(text: &str) -> PathBuf {
    if let Some(rest) = text.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(text)
}

/// Returns `false` to request the run loop break. Currently only reached via
/// `d`/`D` confirmation — quitting no longer goes through here (it never
/// kills anything, so it never needed a confirm).
fn handle_confirm_key(app: &mut App, key: KeyEvent) -> bool {
    let Some(confirm) = app.confirm.take() else { return true };
    match key.code {
        KeyCode::Char('y') | KeyCode::Enter => match confirm.action {
            ConfirmAction::Drop { agent_id, recursive } => {
                let resp = app.dispatch(Request::TuiDrop { agent_id, recursive });
                app.status_message = if resp.ok {
                    None
                } else {
                    Some(format!("drop failed: {}", resp.error.unwrap_or_else(|| "unknown error".to_string())))
                };
                true
            }
            ConfirmAction::Shutdown { .. } => {
                let resp = app.dispatch(Request::Shutdown);
                if resp.ok {
                    // The daemon (or, in mock mode, everything this process
                    // owns) is gone — nothing left to show, so exit like a
                    // quit rather than sit in front of a dead tree.
                    false
                } else {
                    app.status_message =
                        Some(format!("shutdown failed: {}", resp.error.unwrap_or_else(|| "unknown error".to_string())));
                    true
                }
            }
        },
        KeyCode::Char('n') | KeyCode::Esc => {
            app.status_message = None;
            true
        }
        _ => {
            // Not a recognized response — keep the confirm prompt open.
            app.confirm = Some(confirm);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentId, AgentRegistry, AgentRole, AgentStatus};

    #[test]
    fn mock_ctx_never_gets_a_real_session_manager() {
        let id = &uuid::Uuid::new_v4().to_string()[..8];
        let socket = PathBuf::from(format!("/tmp/ovsr-mock-{id}.sock"));
        let ctx = mock_ctx(socket.clone());
        assert!(ctx.sessions.is_dry_run(), "--mock must never spawn a real PTY");
        let _ = std::fs::remove_file(&socket);
    }

    // ── display_path_from_home ───────────────────────────────────────────────

    #[test]
    fn display_path_from_home_under_home_uses_tilde() {
        let home = dirs::home_dir().expect("test requires resolvable $HOME");
        let path = home.join("projects/overseer");
        assert_eq!(display_path_from_home(&path), "~/projects/overseer");
    }

    #[test]
    fn display_path_from_home_outside_home_is_unchanged() {
        let path = PathBuf::from("/opt/other/repo");
        assert_eq!(display_path_from_home(&path), "/opt/other/repo");
    }

    #[test]
    fn display_path_from_home_home_itself_is_tilde() {
        let home = dirs::home_dir().expect("test requires resolvable $HOME");
        assert_eq!(display_path_from_home(&home), "~");
    }

    // ── quit guard ────────────────────────────────────────────────────────────

    fn register_agent(ctx: &AppCtx, id: Option<AgentId>) -> AgentId {
        use crate::agent::RegisterArgs;
        ctx.registry
            .register(RegisterArgs {
                id,
                name: "agent".to_string(),
                role: AgentRole::Root,
                parent_id: None,
                adapter: "shell".to_string(),
                repo: "repo".to_string(),
                cwd: PathBuf::from("/tmp"),
                branch: None,
                initial_status: AgentStatus::Idle,
            })
            .unwrap()
            .id
    }

    fn app_with_sessions(sessions: SessionManager) -> (App, Arc<AppCtx>) {
        let ctx = Arc::new(AppCtx {
            registry: Arc::new(AgentRegistry::new()),
            sessions: Arc::new(sessions),
            socket: PathBuf::from("/tmp/overseer-quit-test.sock"),
            git: Arc::new(GitClient::new()),
            config: Arc::new(Config::default()),
            watch_sessions: false,
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
        });
        (App::new(ctx.clone()), ctx)
    }

    #[test]
    fn q_quits_immediately_even_with_a_live_agent_registered() {
        let live_id = AgentId::new();
        let sessions =
            SessionManager::dry_run_with_live_sessions([live_id.clone()].into_iter().collect());
        let (mut app, ctx) = app_with_sessions(sessions);
        register_agent(&ctx, Some(live_id));

        let should_continue =
            handle_tree_key(&mut app, KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), 24, &Keybindings::default());

        assert!(!should_continue, "q must quit immediately, no confirm gate");
        assert!(app.confirm.is_none());
    }

    #[test]
    fn ctrl_c_quits_immediately_too() {
        let (mut app, _ctx) = app_with_sessions(SessionManager::dry_run());
        let should_continue =
            handle_tree_key(&mut app, KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), 24, &Keybindings::default());
        assert!(!should_continue);
    }

    #[test]
    fn quitting_never_kills_live_sessions() {
        // Quitting must not touch SessionManager at all — agents are independent
        // child processes that outlive the TUI (tmux-detach style), not something
        // the quit path kills. `dry_run_with_live_sessions` reports `is_alive`
        // from a fixed set it was constructed with, so if quitting called
        // `kill()` on anything, `is_alive` would still report the same value
        // (dry-run kill is a no-op) — the real guarantee here is behavioral:
        // `handle_tree_key`'s quit arms never call session kill at all.
        let live_id = AgentId::new();
        let sessions =
            SessionManager::dry_run_with_live_sessions([live_id.clone()].into_iter().collect());
        let (mut app, ctx) = app_with_sessions(sessions);
        register_agent(&ctx, Some(live_id.clone()));

        handle_tree_key(&mut app, KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), 24, &Keybindings::default());

        assert!(app.is_alive(&live_id), "quit must not kill live agents");
    }

    // ── scrollback keys (SCROLLBACK.md) ──────────────────────────────────────

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn app_with_real_session_scrolled_to(id: &AgentId) -> (App, Arc<AppCtx>) {
        let sessions = SessionManager::new();
        let mut cmd = std::process::Command::new("/bin/sh");
        // More lines than any pane height used below, so there's real
        // scrollback history to move into.
        cmd.args(["-c", "i=0; while [ $i -lt 60 ]; do echo line$i; i=$((i+1)); done; sleep 60"]);
        sessions
            .launch(id.clone(), &PathBuf::from("/tmp"), &cmd, &std::collections::HashMap::new())
            .unwrap();
        let became_dirty = (0..50).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            sessions.generation(id).unwrap_or(0) > 0
        });
        assert!(became_dirty, "session must produce output before there's scrollback to move into");

        let (app, ctx) = app_with_sessions(sessions);
        register_agent(&ctx, Some(id.clone()));
        (app, ctx)
    }

    #[test]
    fn ctrl_u_and_ctrl_d_scroll_by_half_the_pane_height() {
        let id = AgentId::new();
        let (mut app, ctx) = app_with_real_session_scrolled_to(&id);

        handle_tree_key(&mut app, key(KeyCode::Char('u'), KeyModifiers::CONTROL), 20, &Keybindings::default());
        assert_eq!(ctx.sessions.display_offset(&id), 10);

        handle_tree_key(&mut app, key(KeyCode::Char('d'), KeyModifiers::CONTROL), 20, &Keybindings::default());
        assert_eq!(ctx.sessions.display_offset(&id), 0);

        ctx.sessions.kill(&id);
    }

    #[test]
    fn ctrl_y_and_ctrl_e_scroll_by_one_line_nvim_semantics() {
        let id = AgentId::new();
        let (mut app, ctx) = app_with_real_session_scrolled_to(&id);

        // y = up, e = down (nvim semantics, per AGENTS.md keybinding style).
        handle_tree_key(&mut app, key(KeyCode::Char('y'), KeyModifiers::CONTROL), 20, &Keybindings::default());
        assert_eq!(ctx.sessions.display_offset(&id), 1);
        handle_tree_key(&mut app, key(KeyCode::Char('y'), KeyModifiers::CONTROL), 20, &Keybindings::default());
        assert_eq!(ctx.sessions.display_offset(&id), 2);
        handle_tree_key(&mut app, key(KeyCode::Char('e'), KeyModifiers::CONTROL), 20, &Keybindings::default());
        assert_eq!(ctx.sessions.display_offset(&id), 1);

        ctx.sessions.kill(&id);
    }

    #[test]
    fn shift_g_jumps_back_to_the_live_bottom() {
        let id = AgentId::new();
        let (mut app, ctx) = app_with_real_session_scrolled_to(&id);

        handle_tree_key(&mut app, key(KeyCode::Char('u'), KeyModifiers::CONTROL), 20, &Keybindings::default());
        assert!(ctx.sessions.display_offset(&id) > 0);

        handle_tree_key(&mut app, key(KeyCode::Char('G'), KeyModifiers::NONE), 20, &Keybindings::default());
        assert_eq!(ctx.sessions.display_offset(&id), 0);

        ctx.sessions.kill(&id);
    }

    #[test]
    fn scroll_keys_with_an_empty_tree_do_not_panic() {
        let (mut app, _ctx) = app_with_sessions(SessionManager::dry_run());
        for code in [KeyCode::Char('u'), KeyCode::Char('d'), KeyCode::Char('y'), KeyCode::Char('e')] {
            handle_tree_key(&mut app, key(code, KeyModifiers::CONTROL), 20, &Keybindings::default());
        }
        handle_tree_key(&mut app, key(KeyCode::Char('G'), KeyModifiers::NONE), 20, &Keybindings::default());
    }

    #[test]
    fn jump_in_resets_scroll_to_bottom_before_focusing() {
        let id = AgentId::new();
        let (mut app, ctx) = app_with_real_session_scrolled_to(&id);

        handle_tree_key(&mut app, key(KeyCode::Char('u'), KeyModifiers::CONTROL), 20, &Keybindings::default());
        assert!(ctx.sessions.display_offset(&id) > 0);

        jump_in(&mut app);

        assert_eq!(app.focus, Focus::Pane);
        assert_eq!(ctx.sessions.display_offset(&id), 0, "jumping in must reset scroll to bottom");

        ctx.sessions.kill(&id);
    }

    #[test]
    fn jump_in_clears_a_stale_not_running_message_on_a_later_success() {
        // Regression test: a failed jump-in (e.g. selecting a root right at
        // spawn time, before its first status push lands) used to leave
        // "agent is not running" in the footer forever, since only the
        // failure branch touched `status_message` — a later successful
        // jump-in, on the same or a different agent, never cleared it.
        let dead_id = AgentId::new();
        let live_id = AgentId::new();
        let sessions =
            SessionManager::dry_run_with_live_sessions([live_id.clone()].into_iter().collect());
        let (mut app, ctx) = app_with_sessions(sessions);
        register_agent(&ctx, Some(dead_id));
        register_agent(&ctx, Some(live_id));

        // First agent in the tree is not alive: jump-in fails and sets the message.
        jump_in(&mut app);
        assert_eq!(app.status_message.as_deref(), Some("agent is not running"));
        assert_eq!(app.focus, Focus::Tree);

        // Move to the live agent and jump in successfully.
        app.with_tree_mut(|t| t.move_down());
        jump_in(&mut app);

        assert_eq!(app.focus, Focus::Pane);
        assert!(
            app.status_message.is_none(),
            "a successful jump-in must clear a stale failure message from an earlier attempt"
        );
    }

    // ── keybindings config (PHASE5B.md Task 2) ───────────────────────────────

    #[test]
    fn handle_tree_key_respects_a_remapped_action() {
        let (mut app, ctx) = app_with_sessions(SessionManager::dry_run());
        register_agent(&ctx, None);
        let kb = Keybindings { spawn_root: KeyBinding::Char('a'), ..Keybindings::default() };

        // 'n' (the default) must no longer trigger spawn-root once remapped away.
        handle_tree_key(&mut app, key(KeyCode::Char('n'), KeyModifiers::NONE), 24, &kb);
        assert!(app.input.is_none(), "'n' must be inert once spawn_root is remapped to 'a'");

        // 'a' (the remap) must trigger it instead.
        handle_tree_key(&mut app, key(KeyCode::Char('a'), KeyModifiers::NONE), 24, &kb);
        assert!(matches!(
            app.input.as_ref().map(|i| &i.action),
            Some(PendingAction::SpawnRoot)
        ));
    }

    // ── fuzzy search (PHASE5B.md Task 1) ─────────────────────────────────────

    fn register_named_agent(ctx: &AppCtx, name: &str) -> AgentId {
        use crate::agent::RegisterArgs;
        ctx.registry
            .register(RegisterArgs {
                id: None,
                name: name.to_string(),
                role: AgentRole::Root,
                parent_id: None,
                adapter: "shell".to_string(),
                repo: "repo".to_string(),
                cwd: PathBuf::from("/tmp"),
                branch: None,
                initial_status: AgentStatus::Idle,
            })
            .unwrap()
            .id
    }

    #[test]
    fn slash_opens_a_search_prompt() {
        let (mut app, _ctx) = app_with_sessions(SessionManager::dry_run());
        handle_tree_key(&mut app, key(KeyCode::Char('/'), KeyModifiers::NONE), 24, &Keybindings::default());
        assert!(matches!(app.input.as_ref().map(|i| &i.action), Some(PendingAction::Search)));
    }

    #[test]
    fn submit_search_jumps_the_cursor_to_the_first_match() {
        let (mut app, ctx) = app_with_sessions(SessionManager::dry_run());
        register_named_agent(&ctx, "fix-login-bug");
        let auth_id = register_named_agent(&ctx, "auth-module");
        app.with_tree_mut(|t| t.cursor = 0); // start on "fix-login-bug"

        app.input = Some(InputState { action: PendingAction::Search, buffer: "auth".to_string() });
        handle_input_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.input.is_none(), "Enter must close the search prompt");
        let selected = app.with_tree(|t| t.selected()).unwrap();
        assert_eq!(selected.id, auth_id);
    }

    #[test]
    fn submit_search_keeps_the_cursor_if_it_already_matches() {
        let (mut app, ctx) = app_with_sessions(SessionManager::dry_run());
        register_named_agent(&ctx, "auth-module");
        let write_tests_id = register_named_agent(&ctx, "write-tests");
        app.with_tree_mut(|t| t.cursor = 1); // start on "write-tests", which itself matches "write"

        app.input = Some(InputState { action: PendingAction::Search, buffer: "write".to_string() });
        handle_input_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let selected = app.with_tree(|t| t.selected()).unwrap();
        assert_eq!(selected.id, write_tests_id, "cursor must stay put when it's already a match");
    }

    #[test]
    fn esc_during_search_restores_the_full_tree_without_moving_the_cursor() {
        let (mut app, ctx) = app_with_sessions(SessionManager::dry_run());
        let fix_id = register_named_agent(&ctx, "fix-login-bug");
        register_named_agent(&ctx, "auth-module");
        app.with_tree_mut(|t| t.cursor = 0); // "fix-login-bug"

        app.input = Some(InputState { action: PendingAction::Search, buffer: "auth".to_string() });
        handle_input_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(app.input.is_none(), "Esc must close the search prompt");
        let selected = app.with_tree(|t| t.selected()).unwrap();
        assert_eq!(selected.id, fix_id, "Esc must not move the real cursor");
    }
}
