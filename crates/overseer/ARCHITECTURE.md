# overseer (binary)

The `overseer` executable: a thin entrypoint over `overseer-core` plus the
ratatui TUI. The binary's name, CLI surface, and behavior are a hard public
contract (installed agent hooks shell out to it; the daemon runs as
`overseer daemon`) — see [../../ARCHITECTURE.md](../../ARCHITECTURE.md) for
the workspace picture and [../../AGENTS.md](../../AGENTS.md) for the product
doc (CLI command table, TUI layout, keybindings, scrollback, search).

## Entrypoint dispatch

`main.rs` (~30 lines) parses args and hands off — all real logic lives
elsewhere:

| Invocation | Runs | Logic lives in |
|------------|------|----------------|
| `overseer` (no subcommand) | the TUI | this crate (`tui.rs`) |
| `overseer --mock` | the TUI on in-process demo data — no daemon, no real PTYs | this crate |
| `overseer daemon` | the daemon process | core (`daemon::run_daemon`) |
| `overseer install/uninstall` | adapter file install | core (`install::run_install`) |
| `overseer kill` | forceful daemon cleanup | core (`kill::run_kill`) |
| everything else (`status`, `spawn`, `list`, …) | one-shot IPC client | `cli.rs` builds the request, core's `ipc::client` sends it |

One subtlety kept in `main.rs` itself: the `pushed_at` timestamp for
`status --from-hook` is captured before clap parsing, so the registry's
staleness guard sees the invocation moment, not parse+transcript-read time.

## Module map

```
src/
├── main.rs           Dispatch above
├── cli.rs            clap definitions + one-shot client glue (build Request, print Response)
├── tui.rs            Event loop: attach connection, key/mouse handling, focus + confirm +
│                     search + picker state machines, notify emission
├── app.rs            App: Backend enum (Mock | Daemon) unifying tree access, session I/O, and
│                     dispatch behind one API — the single place that branches on which backend
│                     is live; tui.rs/ui/ call the same methods either way
└── ui/               Render layer only — no business logic, no state mutation
    ├── mod           Tree|pane split (~25/75): agent tree, detail pane, status bar, spawn
    │                 modal, help popup (generated from the live Keybindings struct)
    └── term_pane     Paints the selected agent's pane cell-by-cell from a GridSnapshot — the
                      only render currency, in both --mock and daemon-attached modes; converts
                      core's ColorDto to ratatui colors via map_dto_color
```

`tests/alacritty_boundary.rs` asserts this crate never imports
`alacritty_terminal` (the twin of core's guard, which carves out
`session/pty.rs`); `tests/kill_daemon_recovery.rs` is the end-to-end
wedged-daemon recovery test.

## Invariants owned by this crate

- **`ui/` renders; it never mutates.** State changes go through
  `App`/`AgentTree`/core.
- **One code path per request regardless of backend** — `Backend::{Mock,
  Daemon}` branching stays inside `app.rs` (bar the one `pane_grid` lookup in
  the run loop, which is ui-shape glue).
- **Keybinding house style is nvim**, and a focused pane intercepts only
  `Ctrl-h` — every other key forwards to the agent. Full rules and the
  remappable-vs-fixed table: AGENTS.md "TUI Layout".
- **Quit is a detach.** `q`/`Ctrl-C` never kills agents or the daemon; only
  `d`/`D`/`Q` (all confirmed) destroy anything, and workspace drops go
  through the TUI-only `Request::TuiDrop` wire request.
