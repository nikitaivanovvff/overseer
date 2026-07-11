# Overseer Architecture

How the workspace is put together and where a given kind of change belongs.
For what Overseer *is* — the product, its mental model, glossary, agent
integration, keybindings, config — see [AGENTS.md](AGENTS.md); this file and
the per-crate docs it links cover code structure only.

- [`crates/overseer-core/ARCHITECTURE.md`](crates/overseer-core/ARCHITECTURE.md) — the library: agent model + registry, sessions/PTYs, IPC, daemon, adapters, config
- [`crates/overseer/ARCHITECTURE.md`](crates/overseer/ARCHITECTURE.md) — the binary: CLI dispatch, TUI, render layer

## Process model

Everything meets at one per-user daemon process and its Unix socket:

```
              ┌──────────────────────────────────────────────────────────┐
              │ daemon process (one per user, detached, auto-spawned)    │
              │   logic: overseer_core::daemon / ipc / agent / session   │
              │   entrypoint: the `overseer daemon` subcommand           │
              │                                                          │
              │   AgentRegistry ── agent tree + status broadcast         │
              │   SessionManager ── one PTY + terminal emulator per agent│
              └───────────────────────────┬──────────────────────────────┘
                                          │ Unix socket, newline-delimited JSON
          ┌───────────────────┬───────────┴────────┬─────────────────────┐
          │                   │                    │                     │
   overseer (TUI)      overseer <cmd>       agent hooks           future desktop app
   attach client       one-shot client      (overseer status,     attach client
   (registry events +  (status/spawn/       spawn, …) — also      (planned crate:
   grid snapshots)     list/drop/…)         one-shot clients      overseer-desktop)
```

The daemon **code** lives in `overseer-core`; the `overseer` binary is merely
the executable that hosts it (`overseer daemon`, hidden from `--help`,
auto-spawned on first attach). Clients never share in-process state with the
daemon — the socket is the only channel, and quitting a client detaches
without touching the daemon or its agents.

`overseer --mock` is the one exception to the diagram: fully in-process demo
data, no daemon, no real PTYs.

## Workspace

| Crate | Kind | Artifact | Contents |
|-------|------|----------|----------|
| [`overseer-core`](crates/overseer-core/ARCHITECTURE.md) | lib | none — internal path dependency, never published | Everything client-agnostic: agent model/registry/lifecycle, session + PTY management, IPC protocol/server/client, daemon bootstrap, adapters + install, git info, config parsing, notify transition logic |
| [`overseer`](crates/overseer/ARCHITECTURE.md) | bin | the `overseer` binary (Homebrew) | Thin entrypoint dispatch (~30-line `main.rs`), CLI argument surface (`cli.rs`), and the ratatui TUI (`tui.rs`, `ui/`, `app.rs`) |

A desktop app crate (`overseer-desktop`) is planned as a third member — another
attach client of the same daemon, reusing `overseer-core` for the protocol
and client plumbing. The `overseer` **binary name is a hard public contract**
regardless of frontends: installed agent hooks shell out to `overseer status`
/ `overseer spawn`, and the daemon is spawned as `overseer daemon` — renaming
the binary breaks every existing install.

## What changes where

The rule of thumb: **if the change should hold for every frontend, it goes in
`overseer-core`; if it's about how a terminal user sees or drives Overseer,
it goes in `overseer`.**

Change in `overseer-core`:
- Agent lifecycle semantics: spawn/drop rules (e.g. "no grandchildren"), status meanings, staleness guards
- Anything about the daemon, the socket, or the wire protocol (`Request`/`Response`/`AttachEvent`/`GridSnapshot`)
- PTY/terminal-emulation behavior (`session/pty.rs` — the only `alacritty_terminal` importer in the workspace)
- Adapters: a new harness, install files, launch commands, hook wiring
- Config file shape and parsing (all sections — even TUI-only ones like `[keybindings]`/`[theme]` parse here so a second frontend never needs a second loader)

Change in `overseer` (bin):
- Rendering: tree rows, badges, detail pane, status bar, modals, the pane painter
- Interaction: keybinding handling, focus model, scrollback UX, mouse handling
- CLI argument surface (flags/subcommands in `cli.rs` — though the request they *send* is core's protocol)
- `App`'s Mock-vs-Daemon glue

Litmus test: "should the future desktop app get this behavior automatically?"
Yes → core. No, it's terminal-specific presentation → bin.

## Cross-crate invariants

Enforced boundaries (the first two have guard tests on both sides):

- `alacritty_terminal` appears **only** in core's `session/pty.rs`. Everything else — including the bin's render layer — consumes `GridSnapshot`/`TermModes` DTOs. Swapping the terminal backend must stay a one-file rewrite.
- No UI toolkit in core: ratatui never appears in `overseer-core` (`config/theme.rs` stores the wire-neutral `ColorDto`, not a ratatui color). The one deliberate exception is crossterm in `session/keys.rs`, kept in core rather than splitting the key encoder from its `KeyEvent` input type.
- `ui/` is a render layer only; state mutations go through `App`/`AgentTree`/`SessionManager`. `App` (bin) is the single place that branches on `Backend::{Mock, Daemon}`.
- Core test helpers cross the crate boundary via the `test-util` feature (`overseer` lists core in `[dev-dependencies]` with it) — never by duplicating helpers or loosening `#[cfg(test)]` to unconditional `pub`.

See AGENTS.md's "Best Practices" / "What to Avoid" for the full list with
rationale.
