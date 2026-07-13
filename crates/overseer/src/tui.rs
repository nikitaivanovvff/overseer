//! TUI controller: event loop + key handlers. `app.rs` owns the pure state
//! (`App`, `Focus`, `InputState`, …); this is its controller — driving
//! `crossterm` events into state mutations and calling `ui::render` each tick.

use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, layout::{Position, Rect}, Terminal};
use std::{io, path::PathBuf, sync::Arc, time::Duration};

use overseer_core::agent::{AgentId, AgentTree};
use crate::app::{
    App, Backend, ConfirmAction, ConfirmState, DaemonState, Focus, InputState, PendingAction,
};
use overseer_core::config::{Action, Config, KeyBinding, Keybindings};
use overseer_core::daemon;
use overseer_core::git::GitClient;
use overseer_core::ipc;
use overseer_core::ipc::protocol::Request;
use overseer_core::ipc::AppCtx;
use overseer_core::session::{self, SessionManager};
use crate::ui;

pub fn run_tui(socket: PathBuf, mock: bool) -> Result<()> {
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        // Mirror the full arming sequence below, bracketed paste included —
        // leaving any of these modes armed past our exit wedges the host
        // terminal (mouse capture especially: every wheel/click keeps
        // emitting escape sequences into whatever shell prompt follows).
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture, DisableBracketedPaste);
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
        registry: Arc::new(overseer_core::agent::AgentRegistry::from_tree(AgentTree::with_mock_data())),
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
    notify_config: &overseer_core::config::NotifyConfig,
    keybindings: &Keybindings,
    theme: &overseer_core::config::Theme,
) -> Result<()> {
    let mut last_pane_size: Option<(u16, u16)> = None;
    let mut last_selected: Option<overseer_core::agent::AgentId> = None;
    let mut last_statuses: std::collections::HashMap<overseer_core::agent::AgentId, overseer_core::agent::AgentStatus> =
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
        let transitions = overseer_core::notify::status_transitions(&last_statuses, &flat);
        overseer_core::notify::handle_transitions(notify_config, &transitions);
        last_statuses = overseer_core::notify::snapshot_statuses(&flat);

        let prompt = build_prompt(app);
        let input = app.input.as_ref();
        let pane_focused = app.focus == Focus::Pane;
        let pane_grid = selected_id.as_ref().and_then(|id| app.pane_grid(id));
        let mut layout = ui::RenderLayout { pane_rect: Rect::default(), tree_rect: Rect::default(), tree_rows: Vec::new() };
        app.with_tree(|tree| {
            terminal.draw(|f| {
                layout = ui::render(
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
        let pane_rect = layout.pane_rect;
        // Every agent shares one PTY size — resize on layout
        // change (including the very first draw, sizing new sessions before
        // the user ever gets to spawn one).
        let pane_size = (pane_rect.width, pane_rect.height);
        if last_pane_size != Some(pane_size) {
            // A zero dimension means the pane isn't visible (a terminal
            // reporting a degenerate size mid-startup/mid-resize) — nothing
            // to size agents to. `SessionManager::resize_all` guards this
            // server-side too (resizing a terminal grid to 0 kills its PTY
            // reader thread and with it exit detection); skipping here just
            // avoids sending a request the daemon would ignore anyway.
            if pane_size.0 > 0 && pane_size.1 > 0 {
                app.resize(pane_size.0 as usize, pane_size.1 as usize);
            }
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
            // Drain *everything* already queued this frame instead of one
            // event per 16ms iteration. A trackpad flick delivers dozens of
            // wheel notches (or, on wheel-as-arrows terminals, synthetic
            // Up/Down keys) nearly at once — at one-per-frame they used to
            // keep draining (and each firing its own `Request::Scroll` with
            // a full ~1MB grid reply) for seconds after the gesture ended.
            // Scroll intents coalesce into one net delta flushed as at most
            // one `App::scroll` per frame; a flick that nets to zero sends
            // nothing at all. Everything else (keys, clicks, paste) is
            // handled in arrival order exactly as before. The cap keeps a
            // pathological flood (e.g. a mouse-motion storm under capture)
            // from starving rendering; leftovers are simply next frame's
            // first drain.
            let mut pending_scroll: i32 = 0;
            let mut quit = false;
            for _ in 0..MAX_EVENTS_PER_FRAME {
                match event::read()? {
                    Event::Key(key) if key.kind != event::KeyEventKind::Release => {
                        if let Some(delta) = tree_arrow_scroll_delta(app, &key) {
                            pending_scroll += delta;
                        } else if app.show_help {
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
                                            quit = true;
                                            break;
                                        }
                                    } else if !handle_tree_key(app, key, pane_rect.height, keybindings) {
                                        quit = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Event::Paste(text) if app.focus == Focus::Pane => forward_paste(app, &text),
                    Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                        handle_left_click(app, &layout, mouse);
                    }
                    Event::Mouse(mouse) => {
                        pending_scroll += handle_mouse_wheel(app, &mouse, pane_rect);
                    }
                    _ => {}
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
            // Flushed against the frame-start selection: if a j/k mid-drain
            // moved the cursor, the scroll lands on the previously selected
            // agent and the next frame's selection-change reset (above)
            // snaps the new one to the live bottom anyway.
            apply_pending_scroll(app, selected_id.as_ref(), pending_scroll);
            if quit {
                break;
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
        Action::SpawnRoot => start_spawn_root(app),
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

/// Left-click is click-to-focus, window-manager style: a click in the tree
/// half focuses the tree (selecting the row it landed on, if any), a click on
/// the pane jumps in — mirroring `Enter`/`o` via `jump_in`, so the alive
/// check and scroll reset live in one place. Works from *either* focus state:
/// intercepting a click while a pane is focused steals nothing from the
/// agent's TUI because Overseer forwards wheel reports, not UI clicks. A click on an already-focused pane
/// is a no-op rather than a re-`jump_in`, so it never resets a wheel-scrolled
/// position back to the live bottom. Clicks while any modal is open (search
/// input, spawn input, drop/shutdown confirm, help popup) stay no-ops,
/// mirroring how keyboard tree nav is unreachable in those states.
fn handle_left_click(app: &mut App, layout: &ui::RenderLayout, mouse: MouseEvent) {
    if app.input.is_some() || app.confirm.is_some() || app.show_help {
        return;
    }
    let pos = Position::new(mouse.column, mouse.row);
    if layout.tree_rect.contains(pos) {
        if let Some(id) = ui::hit_test_tree(layout.tree_rect, &layout.tree_rows, mouse.column, mouse.row) {
            app.with_tree_mut(|t| t.select_by_id(&id));
        }
        app.focus = Focus::Tree;
    } else if layout.pane_rect.contains(pos) && app.focus != Focus::Pane {
        jump_in(app);
    }
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

/// Lines moved per wheel notch — matches the typical terminal/GUI default
/// (3 lines), not tied to `Ctrl-y`/`Ctrl-e`'s single-line nvim semantics.
const MOUSE_SCROLL_LINES: i32 = 3;

/// Upper bound on input events drained in one frame of `run_app`'s loop.
/// High enough that no human-generated burst ever hits it (a very fast
/// trackpad flick is a few dozen events), low enough that a pathological
/// flood can't postpone rendering indefinitely — anything left over is
/// simply the first thing read next frame, 16ms later.
const MAX_EVENTS_PER_FRAME: usize = 128;

/// The mouse wheel's contribution to this frame's pending scroll delta —
/// positive is up into history, matching `App::scroll`. Zero for anything
/// that isn't a wheel event over the pane rect (clicks are handled
/// separately; a wheel over the tree half is ignored).
///
/// Tree-focused wheel events scroll Overseer's terminal history. Focused
/// wheel events are instead forwarded to an inner TUI that requested mouse
/// reporting, letting it scroll its own alternate-screen conversation.
///
/// Returning a delta instead of applying it immediately is deliberate: the
/// event-drain loop in `run_app` sums every scroll intent in the frame and
/// flushes once (`apply_pending_scroll`), so a trackpad flick costs one
/// `Request::Scroll` instead of one per notch — each of which used to pull
/// a full ~1MB grid snapshot back over the attach connection (a real,
/// reported TUI-freezing flood).
fn wheel_scroll_delta(mouse: &MouseEvent, pane_rect: Rect) -> i32 {
    if !pane_rect.contains(Position::new(mouse.column, mouse.row)) {
        return 0;
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => MOUSE_SCROLL_LINES,
        MouseEventKind::ScrollDown => -MOUSE_SCROLL_LINES,
        _ => 0,
    }
}

fn handle_mouse_wheel(app: &mut App, mouse: &MouseEvent, pane_rect: Rect) -> i32 {
    let direction = match mouse.kind {
        MouseEventKind::ScrollUp => overseer_core::session::keys::WheelDirection::Up,
        MouseEventKind::ScrollDown => overseer_core::session::keys::WheelDirection::Down,
        _ => return 0,
    };
    let position = Position::new(mouse.column, mouse.row);
    if !pane_rect.contains(position) {
        return 0;
    }

    if app.focus == Focus::Pane {
        let Some(id) = app.with_tree(|tree| tree.selected()).map(|node| node.id) else { return 0 };
        let mode = app.term_modes(&id);
        if mode.mouse_reporting {
            let column = mouse.column - pane_rect.x;
            let row = mouse.row - pane_rect.y;
            if let Some(bytes) = overseer_core::session::keys::encode_mouse_wheel(
                direction,
                column,
                row,
                mouse.modifiers,
                mode,
            ) {
                app.write_input(&id, bytes);
            }
            return 0;
        }
    }

    wheel_scroll_delta(mouse, pane_rect)
}

/// The keyboard-arrow contribution to this frame's pending scroll delta:
/// while the *tree* is focused with no modal open, a plain `Up`/`Down`
/// scrolls the selected agent's pane preview one wheel notch, exactly like
/// the mouse wheel — `None` means "not a scroll intent, dispatch normally".
///
/// This exists for terminals that never deliver a real xterm wheel report
/// even with mouse capture armed and instead translate wheel motion into
/// synthetic arrow-key presses (macOS Terminal.app; Kaku's WezTerm-derived
/// alternate-scroll path) — routing tree-focus arrows to preview scroll
/// makes the trackpad work there for free. It's safe within the house
/// rules because arrows were previously *unbound* in tree focus (`j`/`k`
/// is the navigation vocabulary, `key_binding_from_event` doesn't even map
/// arrow keys), so nothing is stolen — and it changes nothing while a pane
/// is focused: there arrows still forward to the agent untouched (`Ctrl-h`
/// stays the only intercepted key, pinned by
/// `arrow_keys_forward_to_the_agent_while_pane_is_focused_not_scroll`).
fn tree_arrow_scroll_delta(app: &App, key: &KeyEvent) -> Option<i32> {
    if app.focus != Focus::Tree
        || app.show_help
        || app.input.is_some()
        || app.confirm.is_some()
        || key.modifiers != KeyModifiers::NONE
    {
        return None;
    }
    match key.code {
        KeyCode::Up => Some(MOUSE_SCROLL_LINES),
        KeyCode::Down => Some(-MOUSE_SCROLL_LINES),
        _ => None,
    }
}

/// Flushes one frame's coalesced scroll delta to the selected agent — the
/// single `App::scroll` call a whole frame's worth of wheel notches and
/// synthetic arrows collapses into. A net-zero delta (or no selection)
/// sends nothing at all.
fn apply_pending_scroll(app: &mut App, selected_id: Option<&AgentId>, delta: i32) {
    if delta == 0 {
        return;
    }
    let Some(id) = selected_id else { return };
    app.scroll(id, delta);
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

/// `n`: opens the repo-path prompt for a new workspace — always a bare shell
/// in the chosen repo, asking for nothing but the directory. Run your own
/// agent inside it whenever you're ready.
fn start_spawn_root(app: &mut App) {
    app.status_message = None;
    let default_cwd = std::env::current_dir().map(|p| display_path_from_home(&p)).unwrap_or_default();
    app.input = Some(InputState { action: PendingAction::SpawnRoot, buffer: default_cwd });
}

/// `s`: opens a name prompt for a taskless child under the selected node. Whether
/// it's actually eligible to take a child is decided by the server's spawn handler
/// alone (AGENTS.md: depth/cap admission lives there, "not in the TUI, not as
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
            let name = input.buffer.trim().to_string();
            if name.is_empty() {
                app.status_message = Some("spawn cancelled: empty name".to_string());
                return;
            }
            dispatch_and_report(app, Request::TuiSpawnChild { parent_id, name });
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
    use overseer_core::agent::{AgentId, AgentRegistry, AgentRole, AgentStatus};

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
        use overseer_core::agent::RegisterArgs;
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

    // ── mouse wheel scrollback (SCROLLBACK.md) ───────────────────────────────
    //
    // Tree focus and focused shells without mouse reporting use Overseer's
    // history. A focused TUI that requested mouse reports receives the wheel
    // itself, covered by the mode/encoder tests in overseer-core.

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }
    }

    /// One wheel notch, end to end, exactly as `run_app`'s drain-then-flush
    /// composes it in production: extract the event's delta, flush it as the
    /// frame's pending scroll.
    fn wheel(app: &mut App, ev: MouseEvent, pane_rect: Rect, selected_id: Option<&AgentId>) {
        let delta = handle_mouse_wheel(app, &ev, pane_rect);
        apply_pending_scroll(app, selected_id, delta);
    }

    #[test]
    fn mouse_scroll_up_moves_into_history_while_pane_is_focused() {
        let id = AgentId::new();
        let (mut app, ctx) = app_with_real_session_scrolled_to(&id);
        jump_in(&mut app);
        assert_eq!(app.focus, Focus::Pane);

        let pane_rect = Rect::new(0, 0, 80, 20);
        wheel(&mut app, mouse(MouseEventKind::ScrollUp, 10, 10), pane_rect, Some(&id));

        assert_eq!(ctx.sessions.display_offset(&id), MOUSE_SCROLL_LINES as usize);
        ctx.sessions.kill(&id);
    }

    #[test]
    fn mouse_scroll_down_moves_back_toward_live() {
        let id = AgentId::new();
        let (mut app, ctx) = app_with_real_session_scrolled_to(&id);
        jump_in(&mut app);

        let pane_rect = Rect::new(0, 0, 80, 20);
        wheel(&mut app, mouse(MouseEventKind::ScrollUp, 10, 10), pane_rect, Some(&id));
        wheel(&mut app, mouse(MouseEventKind::ScrollUp, 10, 10), pane_rect, Some(&id));
        wheel(&mut app, mouse(MouseEventKind::ScrollDown, 10, 10), pane_rect, Some(&id));

        assert_eq!(ctx.sessions.display_offset(&id), MOUSE_SCROLL_LINES as usize);
        ctx.sessions.kill(&id);
    }

    #[test]
    fn mouse_scroll_outside_the_pane_rect_is_ignored() {
        let id = AgentId::new();
        let (mut app, ctx) = app_with_real_session_scrolled_to(&id);
        jump_in(&mut app);

        // Pane occupies columns/rows [0,80)x[0,20) — (90, 25) is outside it
        // (e.g. over the tree half or the status bar).
        let pane_rect = Rect::new(0, 0, 80, 20);
        wheel(&mut app, mouse(MouseEventKind::ScrollUp, 90, 25), pane_rect, Some(&id));

        assert_eq!(ctx.sessions.display_offset(&id), 0, "scroll outside the pane must not move it");
        ctx.sessions.kill(&id);
    }

    #[test]
    fn mouse_scroll_also_works_from_tree_focus_preview() {
        let id = AgentId::new();
        let (mut app, ctx) = app_with_real_session_scrolled_to(&id);
        assert_eq!(app.focus, Focus::Tree);

        let pane_rect = Rect::new(0, 0, 80, 20);
        wheel(&mut app, mouse(MouseEventKind::ScrollUp, 10, 10), pane_rect, Some(&id));

        assert_eq!(ctx.sessions.display_offset(&id), MOUSE_SCROLL_LINES as usize);
        ctx.sessions.kill(&id);
    }

    #[test]
    fn mouse_scroll_with_no_selection_does_not_panic() {
        let (mut app, _ctx) = app_with_sessions(SessionManager::dry_run());
        let pane_rect = Rect::new(0, 0, 80, 20);
        wheel(&mut app, mouse(MouseEventKind::ScrollUp, 10, 10), pane_rect, None);
    }

    #[test]
    fn arrow_keys_forward_to_the_agent_while_pane_is_focused_not_scroll() {
        // Some terminals (macOS Terminal.app notably) translate mouse-wheel
        // scroll into synthetic Up/Down key sequences instead of a real
        // xterm mouse report — and those synthetic sequences are byte-for-
        // byte indistinguishable here from a genuine Up/Down keypress
        // (`session::keys::encode_key` already forwards both as real xterm
        // escape sequences, see its own tests). Many agent TUIs rely on
        // plain arrow keys for their own history/menu navigation, so
        // binding them to scroll here would silently break that for every
        // agent whenever the pane is focused — not just on the terminals
        // this was meant to help. `handle_pane_key` deliberately leaves
        // Up/Down unhandled, same as every other non-`Ctrl-h` key; this
        // pins that decision against a future "helpful" regression. See
        // AGENTS.md "Scrollback" for the caveat this leaves affected users
        // with (drop to tree focus for a keyboard-only fallback).
        let id = AgentId::new();
        let (mut app, ctx) = app_with_real_session_scrolled_to(&id);
        jump_in(&mut app);
        assert_eq!(app.focus, Focus::Pane);

        handle_pane_key(&mut app, key(KeyCode::Up, KeyModifiers::NONE));
        handle_pane_key(&mut app, key(KeyCode::Down, KeyModifiers::NONE));

        assert_eq!(app.focus, Focus::Pane, "arrow keys must not change focus");
        assert_eq!(
            ctx.sessions.display_offset(&id),
            0,
            "arrow keys must not scroll — they forward to the agent's PTY untouched"
        );

        ctx.sessions.kill(&id);
    }

    /// The tree-focus complement to the pinned pane-focus test above: while
    /// the tree is focused the pane is a read-only preview (nothing forwards
    /// to the agent), so plain Up/Down — previously unbound there — safely
    /// become one-wheel-notch preview scrolls. This is the whole fallback
    /// for terminals that translate wheel motion into synthetic arrow keys
    /// (Terminal.app, Kaku's alternate-scroll path): their "wheel" now
    /// scrolls the preview exactly like a real wheel report would.
    #[test]
    fn tree_focus_arrow_keys_scroll_the_selected_pane_preview() {
        let id = AgentId::new();
        let (mut app, ctx) = app_with_real_session_scrolled_to(&id);
        assert_eq!(app.focus, Focus::Tree);

        let up = tree_arrow_scroll_delta(&app, &key(KeyCode::Up, KeyModifiers::NONE))
            .expect("Up in tree focus is a scroll intent");
        apply_pending_scroll(&mut app, Some(&id), up);
        assert_eq!(ctx.sessions.display_offset(&id), MOUSE_SCROLL_LINES as usize);

        let down = tree_arrow_scroll_delta(&app, &key(KeyCode::Down, KeyModifiers::NONE))
            .expect("Down in tree focus is a scroll intent");
        apply_pending_scroll(&mut app, Some(&id), down);
        assert_eq!(ctx.sessions.display_offset(&id), 0, "Down scrolls back toward live");

        ctx.sessions.kill(&id);
    }

    #[test]
    fn arrow_scroll_intent_is_suppressed_while_focused_modal_or_modified() {
        let (mut app, _ctx) = app_with_sessions(SessionManager::dry_run());
        let up = key(KeyCode::Up, KeyModifiers::NONE);

        app.focus = Focus::Pane;
        assert_eq!(tree_arrow_scroll_delta(&app, &up), None, "pane focus: arrows forward to the agent");
        app.focus = Focus::Tree;

        assert!(tree_arrow_scroll_delta(&app, &up).is_some(), "sanity: plain tree focus is a scroll intent");

        app.show_help = true;
        assert_eq!(tree_arrow_scroll_delta(&app, &up), None, "help popup: any key must close it instead");
        app.show_help = false;

        app.input = Some(InputState { action: PendingAction::Search, buffer: String::new() });
        assert_eq!(tree_arrow_scroll_delta(&app, &up), None, "open input prompt: keys belong to it");
        app.input = None;

        assert_eq!(
            tree_arrow_scroll_delta(&app, &key(KeyCode::Up, KeyModifiers::SHIFT)),
            None,
            "modified arrows are not wheel translations — leave them unbound"
        );
    }

    /// One frame's worth of wheel notches (or synthetic arrows) collapses
    /// into a single flushed delta — and a flick that nets to zero flushes
    /// nothing at all. This is the client half of the scroll-flood fix: per
    /// notch, the old path sent one `Request::Scroll` and got a full ~1MB
    /// grid back, which a trackpad flick turned into seconds of queued
    /// serialize/parse work (a real, reported TUI freeze).
    #[test]
    fn a_frames_scroll_intents_coalesce_into_one_net_delta() {
        let id = AgentId::new();
        let (mut app, ctx) = app_with_real_session_scrolled_to(&id);
        let pane_rect = Rect::new(0, 0, 80, 20);

        // up + up + down, all in one frame → one flush of net +3.
        let mut pending = 0;
        pending += wheel_scroll_delta(&mouse(MouseEventKind::ScrollUp, 10, 10), pane_rect);
        pending += wheel_scroll_delta(&mouse(MouseEventKind::ScrollUp, 10, 10), pane_rect);
        pending += wheel_scroll_delta(&mouse(MouseEventKind::ScrollDown, 10, 10), pane_rect);
        assert_eq!(pending, MOUSE_SCROLL_LINES);
        apply_pending_scroll(&mut app, Some(&id), pending);
        assert_eq!(ctx.sessions.display_offset(&id), MOUSE_SCROLL_LINES as usize);

        // A net-zero flick flushes nothing (delta 0 short-circuits before
        // any backend call — in daemon mode that's "no request at all").
        let mut pending = 0;
        pending += wheel_scroll_delta(&mouse(MouseEventKind::ScrollUp, 10, 10), pane_rect);
        pending += wheel_scroll_delta(&mouse(MouseEventKind::ScrollDown, 10, 10), pane_rect);
        assert_eq!(pending, 0);
        apply_pending_scroll(&mut app, Some(&id), pending);
        assert_eq!(ctx.sessions.display_offset(&id), MOUSE_SCROLL_LINES as usize, "net-zero must move nothing");

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
        assert!(matches!(app.input.as_ref().map(|i| &i.action), Some(PendingAction::SpawnRoot)));
    }

    // ── root-spawn prompt (`n`) ───────────────────────────────────────────────

    #[test]
    fn start_spawn_root_opens_the_repo_path_prompt_directly() {
        let (mut app, _ctx) = app_with_sessions(SessionManager::dry_run());
        start_spawn_root(&mut app);
        assert!(matches!(app.input.as_ref().map(|i| &i.action), Some(PendingAction::SpawnRoot)));
    }

    // ── fuzzy search (PHASE5B.md Task 1) ─────────────────────────────────────

    fn register_named_agent(ctx: &AppCtx, name: &str) -> AgentId {
        use overseer_core::agent::RegisterArgs;
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

    // ── mouse click-to-focus ─────────────────────────────────────────────────

    fn mouse_down(column: u16, row: u16) -> MouseEvent {
        MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column, row, modifiers: KeyModifiers::NONE }
    }

    /// A tree|pane layout with two live root agents registered — mirrors a
    /// real frame's `RenderLayout`, but built by hand so the test doesn't
    /// need a real terminal. Tree rows land at screen rows 1 and 2 (row 0 is
    /// the `List` border `render_agent_tree` always draws); the pane sits to
    /// the tree's right, columns [30, 80).
    fn app_with_two_live_agents_and_layout() -> (App, Arc<AppCtx>, AgentId, AgentId, ui::RenderLayout) {
        let id_a = AgentId::new();
        let id_b = AgentId::new();
        let sessions = SessionManager::dry_run_with_live_sessions(
            [id_a.clone(), id_b.clone()].into_iter().collect(),
        );
        let (mut app, ctx) = app_with_sessions(sessions);
        register_agent(&ctx, Some(id_a.clone()));
        register_agent(&ctx, Some(id_b.clone()));
        app.with_tree_mut(|t| t.cursor = 0);

        let tree_rect = Rect { x: 0, y: 0, width: 30, height: 10 };
        let pane_rect = Rect { x: 30, y: 0, width: 50, height: 10 };
        let tree_rows = vec![id_a.clone(), id_b.clone()];
        (app, ctx, id_a, id_b, ui::RenderLayout { pane_rect, tree_rect, tree_rows })
    }

    #[test]
    fn left_click_on_a_tree_row_selects_it_without_jumping_in() {
        let (mut app, ctx, id_a, id_b, layout) = app_with_two_live_agents_and_layout();

        // Second row ("id_b") sits at screen row 2.
        handle_left_click(&mut app, &layout, mouse_down(5, 2));

        assert_eq!(app.with_tree(|t| t.selected()).map(|n| n.id), Some(id_b.clone()));
        assert_eq!(app.focus, Focus::Tree, "a tree click focuses the tree, it doesn't jump in");

        ctx.sessions.kill(&id_a);
        ctx.sessions.kill(&id_b);
    }

    #[test]
    fn left_click_on_the_pane_jumps_in() {
        let (mut app, ctx, id_a, id_b, layout) = app_with_two_live_agents_and_layout();
        assert_eq!(app.focus, Focus::Tree);

        handle_left_click(&mut app, &layout, mouse_down(45, 5));

        assert_eq!(app.focus, Focus::Pane, "a pane click must jump in, like Enter/o");
        assert_eq!(
            app.with_tree(|t| t.selected()).map(|n| n.id),
            Some(id_a.clone()),
            "a pane click must not move the tree cursor"
        );

        ctx.sessions.kill(&id_a);
        ctx.sessions.kill(&id_b);
    }

    #[test]
    fn left_click_on_the_pane_with_a_dead_agent_stays_in_the_tree() {
        let id = AgentId::new();
        // No live sessions at all — the selected agent is dead.
        let (mut app, ctx) = app_with_sessions(SessionManager::dry_run());
        register_agent(&ctx, Some(id.clone()));
        app.with_tree_mut(|t| t.cursor = 0);
        let layout = ui::RenderLayout {
            pane_rect: Rect { x: 30, y: 0, width: 50, height: 10 },
            tree_rect: Rect { x: 0, y: 0, width: 30, height: 10 },
            tree_rows: vec![id],
        };

        handle_left_click(&mut app, &layout, mouse_down(45, 5));

        assert_eq!(app.focus, Focus::Tree, "can't focus a dead agent's pane");
        assert!(app.status_message.is_some(), "same feedback as Enter on a dead agent");
    }

    #[test]
    fn click_outside_both_rects_is_a_noop() {
        let (mut app, ctx, id_a, id_b, layout) = app_with_two_live_agents_and_layout();
        let before = app.with_tree(|t| t.selected()).map(|n| n.id);

        // Row 50 is well past both rects' 10-row height (e.g. the status bar).
        handle_left_click(&mut app, &layout, mouse_down(5, 50));

        assert_eq!(app.with_tree(|t| t.selected()).map(|n| n.id), before);
        assert_eq!(app.focus, Focus::Tree, "a miss must not jump into any pane");

        ctx.sessions.kill(&id_a);
        ctx.sessions.kill(&id_b);
    }

    #[test]
    fn tree_click_while_pane_is_focused_returns_focus_to_the_tree_and_selects() {
        let (mut app, ctx, id_a, id_b, layout) = app_with_two_live_agents_and_layout();
        app.focus = Focus::Pane; // as if the user had already jumped in

        // Click-to-focus works from a focused pane too: UI clicks never forward
        // to the agent, so intercepting this
        // steals nothing — unlike keys, which stay passthrough-only.
        handle_left_click(&mut app, &layout, mouse_down(5, 2));

        assert_eq!(
            app.with_tree(|t| t.selected()).map(|n| n.id),
            Some(id_b.clone()),
            "a tree click while jumped in must select the clicked row"
        );
        assert_eq!(app.focus, Focus::Tree, "…and hand focus back to the tree");

        ctx.sessions.kill(&id_a);
        ctx.sessions.kill(&id_b);
    }

    /// The `app.focus != Focus::Pane` guard: re-clicking an already-focused
    /// pane must not re-run `jump_in`, whose scroll reset would silently
    /// yank a wheel-scrolled view back to the live bottom.
    #[test]
    fn click_on_an_already_focused_pane_does_not_reset_scroll() {
        let id = AgentId::new();
        let (mut app, ctx) = app_with_real_session_scrolled_to(&id);
        jump_in(&mut app);
        assert_eq!(app.focus, Focus::Pane);

        let pane_rect = Rect { x: 30, y: 0, width: 50, height: 10 };
        let layout = ui::RenderLayout {
            pane_rect,
            tree_rect: Rect { x: 0, y: 0, width: 30, height: 10 },
            tree_rows: vec![id.clone()],
        };
        wheel(&mut app, mouse(MouseEventKind::ScrollUp, 45, 5), pane_rect, Some(&id));
        let scrolled = ctx.sessions.display_offset(&id);
        assert!(scrolled > 0, "wheel must have scrolled into history first");

        handle_left_click(&mut app, &layout, mouse_down(45, 5));

        assert_eq!(app.focus, Focus::Pane);
        assert_eq!(ctx.sessions.display_offset(&id), scrolled, "click must not reset the scroll");

        ctx.sessions.kill(&id);
    }

    #[test]
    fn click_is_ignored_while_a_modal_is_open() {
        let (mut app, ctx, id_a, id_b, layout) = app_with_two_live_agents_and_layout();
        app.input = Some(InputState { action: PendingAction::Search, buffer: String::new() });

        handle_left_click(&mut app, &layout, mouse_down(5, 2));

        assert_eq!(app.with_tree(|t| t.selected()).map(|n| n.id), Some(id_a.clone()));
        assert_eq!(app.focus, Focus::Tree);

        ctx.sessions.kill(&id_a);
        ctx.sessions.kill(&id_b);
    }
}
