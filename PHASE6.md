# Phase 6 — Terminal backend switch: tmux → alacritty_terminal

> Implementation plan for the decision recorded in
> `RESEARCH_TERMINAL_BACKEND_FINDINGS.md`: replace the tmux backend with an
> in-process PTY + terminal-emulation layer built on **`alacritty_terminal`
> 0.26** (Apache-2.0, the emulation core of Alacritty and of Zed's embedded
> terminal). Read `AGENTS.md` first; where this plan and it conflict (the tmux
> sections), this plan wins and Task 4 updates `AGENTS.md` to match.

---

## 1. Goal & non-goals

After this phase:

- Overseer is a **single ordinary alt-screen ratatui app**. No tmux server, no
  bootstrap re-exec, no nested clients, no prefix keys anywhere. `tmux` is
  removed from runtime dependencies.
- Each agent is a **PTY owned by Overseer**, with a live in-process terminal
  emulator (`alacritty_terminal::Term`) maintaining its full screen state.
- The right panel renders the **selected agent's grid** directly via ratatui —
  the same path serves read-only preview (tree focused) and interactive
  jump-in (pane focused, keys forwarded to the PTY).
- All keybindings are ours, including the jump-out key.

### Non-goals (explicitly deferred, per the research decision)
- **Persistence.** v1 regression, accepted: agents die when the Overseer
  process exits. A later phase adds the daemon split (research doc §hybrids,
  path (a)). This phase only adds a quit guard so nobody loses agents by
  accident.
- **Mouse forwarding, copy/paste, scrollback view.** Keys-in /
  faithful-rendering-out is the v1 bar. The emulator already *maintains*
  scrollback state; we just don't build UI for it yet.
- **Windows.** macOS + Linux, as today.

---

## 2. Chosen approach & the one real unknown

`alacritty_terminal` gives us, per agent: PTY spawn with env/cwd (its `tty`
module — no `portable-pty` needed), a parser + full grid state (`Term`), and —
critically — **auto-generated replies to terminal queries** (device
attributes, DSR, mode reports) surfaced as events we write back to the PTY.
The pieces *we* write: a session manager (~250 lines), a grid→ratatui
renderer (~400 lines), a key→bytes input encoder (~350 lines), and resize
plumbing. Zed's `crates/terminal` is the reference embedding for all four.

**The one genuinely uncertain mechanic** is faithful *interactive* operation
of Claude Code inside our embedded emulator: query traffic satisfied, key
encoding complete enough to type/approve/steer, wide-char rendering correct.
**Task 0 is a throwaway spike of exactly that** (§5). If it fails, stop —
fallback is "stay on tmux, move the return key to a root-table binding", not
Zellij (see findings doc).

### Locked decisions (don't relitigate mid-build)
- **Crate:** `alacritty_terminal = "0.26"`. Not libghostty-vt (unstable API,
  `!Send`, query replies on us), not vt100/tui-term (parse-only).
- **`TERM=xterm-256color`** for spawned agents — universally present
  terminfo; do NOT use `TERM=alacritty` (its terminfo isn't installed
  everywhere).
- **Pane navigation: nvim-style `Ctrl-h` / `Ctrl-l`** (left → tree,
  right → pane), matching the horizontal tree|pane split. This is an
  instance of the project-wide rule (now in `AGENTS.md`): **nvim-style
  navigation is the preferred keybinding vocabulary** — extend it, don't
  invent a parallel scheme. `Enter`/`o` on the
  tree still jumps in. While the pane is focused, **`Ctrl-h` is the only
  intercepted key** — everything else, modifiers included, forwards to the
  agent. Why these two are safe: `Ctrl-l` is only bound while the *tree* is
  focused, so it's never stolen from the agent; `Ctrl-h` (`^H`) is
  interceptable because modern terminals send `DEL` (0x7f) for Backspace, so
  the agent's backspace still works — validate in Task 0. **Do not use
  `Ctrl-j`/`Ctrl-k` for this:** `Ctrl-j` is LF on the wire and Claude Code's
  own insert-newline binding; stealing it breaks multi-line input. Keep the
  keys as consts so 5b's config phase can expose them.
- **Uniform PTY size:** every agent PTY is sized to the live-pane rect (all
  agents render into the same rect when selected), resized together on
  layout/terminal resize. No per-agent sizes.
- **Scrollback cap:** configure `Term` history to a fixed bound (e.g. 10k
  lines) so N long-running agents don't grow unbounded.

---

## 3. Architecture changes

### 3.1 `session/` — `TmuxClient` out, `SessionManager` in

New `session/pty.rs`, replacing `session/tmux.rs` as the only
terminal-backend boundary (the AGENTS.md rule carries over with a new name:
**no `alacritty_terminal` imports outside `session/` and the pane renderer**).

Per-agent `PtySession` bundles what the crate gives us:

- `tty::new(&Options { shell, working_directory, env, .. }, window_size, id)`
  — replaces `tmux new-session -e K=V -c cwd -- cmd`. Adapter
  `spawn_command`/`env_inject` output maps directly onto `Options`.
- `Term::new(config, &size, proxy)` behind the crate's `FairMutex`, plus
  `EventLoop` (one reader thread per agent) and its `Notifier` for writes.
- An `EventListener` proxy (Send + Clone, mpsc into the app) forwarding:
  - `PtyWrite(payload)` → write back to the PTY via the Notifier (this is the
    query-reply loop — wire it *inside* the session, invisible to the app);
  - child-exit → session-dead notification (exact event name/shape: confirm
    in Task 0 against 0.26's API);
  - `Wakeup` → coalesced dirty flag (the UI loop already ticks at 100ms;
    don't build a wakeup-driven redraw system now).

`SessionManager` (the `AppCtx` handle, replacing `Arc<TmuxClient>`):

- `launch(id, cwd, cmd, env) -> Result<()>` — spawn + register.
- `kill(id)`, `is_alive(id)`, `resize_all(cols, rows)`.
- `write(id, bytes)` — the input path.
- `with_term(id, f)` — brief lock for rendering the selected agent.
- `drain_exits() -> Vec<AgentId>` — consumed by the watcher (3.4).
- **Mock mode:** keep the exact dry-run test surface `TmuxClient` has today
  (`dry_run()`, `dry_run_failing_launch()`, `dry_run_with_live_sessions()`),
  same semantics, so `spawn.rs`/`drop.rs`/`handlers.rs` tests port ~verbatim.

`tmux_session_name()` disappears; sessions are keyed by `AgentId` directly.

### 3.2 `main.rs` — deletions mostly

- **Delete:** `bootstrap_tmux` + `OVERSEER_TUI_HOSTED` re-exec dance,
  `PLACEHOLDER_SESSION` + `ensure_placeholder_session`, `setup_live_pane`,
  `retarget_live_pane` + its respawn self-heal, `jump_in` via `select_pane`,
  `disable_status_bar`. The size-mismatch draw-ordering constraint (the
  comment block in `run_tui`) evaporates — there is no pre-split resize.
- The empty-selection state is a ratatui placeholder widget ("no agent
  selected"), not a fake session.
- Quit guard: `q`/`Ctrl-C` with live agents → reuse `ConfirmState` mechanics
  ("N agents are running and will be killed — quit? (y/n)").

### 3.3 `app.rs` + input routing — the focus model

- `live_pane_id/tty/target: Option<String>` → `focus: Focus::Tree |
  Focus::Pane`.
- Event loop routing: `Focus::Pane` sends every key except `Ctrl-h` through
  the encoder to `sessions.write(selected_id, bytes)`; `Ctrl-h` returns to
  `Focus::Tree`. From the tree, `Ctrl-l` (or `Enter`/`o`) on a selected live
  agent → `Focus::Pane`. Existing tree/input/confirm handling is untouched
  under `Focus::Tree`.
- New `session/keys.rs` (or `input/` module): crossterm `KeyEvent` → escape
  bytes. Must respect terminal modes read from the `Term` (application cursor
  keys, bracketed paste); crib structure from Zed's input translation. This
  is the one component with no crate to lean on — budget the ~350 lines and
  test it hardest.

### 3.4 IPC / watcher — event-driven instead of polling

`ipc/server.rs`'s dead-session watcher currently polls
`tmux.session_exists()` per agent. Replace with `sessions.drain_exits()` →
mark Error (or Done — keep today's semantics: whatever the watcher does now,
preserve it). `drop.rs` swaps `kill_session(name)` for `sessions.kill(id)`;
best-effort semantics ("already dead is not an error") carry over.

### 3.5 `ui/` — the pane becomes real ratatui

New `ui/term_pane.rs`: a widget that locks the selected agent's `Term`,
iterates `renderable_content()` cells into the target `Rect`'s `Buffer` —
fg/bg/underline/bold/italic/inverse mapping, **wide chars occupy two cells
(render the spacer cell empty — this is the classic off-by-one source)**,
cursor drawn only when `Focus::Pane`. The existing 25/75 `Layout` in
`ui/mod.rs` stays; the right side is now this widget instead of empty space
behind a real tmux pane.

Render cost note: only the *selected* agent is rendered; all agents' parsers
stay current on their own threads regardless, so preview switch is instant.

### 3.6 Out-of-scope surfaces that must not break

- `overseer teach` / adapters / hooks: no tmux involvement — untouched.
- Status stays **push** via IPC (AGENTS.md rule). Do not start inferring
  status from PTY output.
- `--mock` stays fully offline: mock `SessionManager`, no PTYs.

---

## 4. Task breakdown

### Task 0 — throwaway spike (gate for everything else)
`examples/pty_spike.rs` (or a `--spike` hidden flag): dummy 25% sidebar +
75% pane; spawn real `claude` via `tty::new` with injected env; render grid;
forward keys when pane-focused; `Ctrl-h`/`Ctrl-l` to swap focus; propagate
resize.

**Go/no-go checklist** (run against real Claude Code, per project memory —
never just mock data):
1. Full-screen UI renders correctly: colors, spinner, alt-screen enter/exit,
   redraws under fast output. Compare side-by-side with a real terminal.
2. Interactive: type a prompt, arrow through a permission dialog, approve it,
   Esc works, `Ctrl-C` reaches the agent.
3. Resize the outer terminal → agent reflows, no corruption.
4. Wide chars/emoji in agent output don't shear columns.
5. Confirm the 0.26 API names assumed in §3.1 (child-exit event, `PtyWrite`
   handling, `Options` env field) and note deviations in the PR.
6. CPU sanity: idle spike process near-zero; chatty output doesn't peg a core.

Also prototype-grade decisions to confirm here: `Ctrl-h` interception is
harmless inside Claude Code (Backspace still deletes — it sends `DEL`, not
`^H`; also check `Ctrl-h` wasn't itself a Claude Code binding worth keeping,
and that `Ctrl-j` newline still reaches it), plus cursor rendering approach. **If any of 1–3 fails and can't be fixed inside the
spike, stop and report — fallback per findings doc.**

### Task 1 — `session/pty.rs` + swap the boundary
`SessionManager` per §3.1 with mock mode; `AppCtx.tmux` → `AppCtx.sessions`;
port `spawn.rs`, `drop.rs`, `handlers.rs`, watcher (§3.4). All existing
lifecycle tests green against the mock backend before any UI work. tmux code
still present but unused after this task (deleted in Task 4, so the tree
bisects).

### Task 2 — grid renderer + read-only preview
`ui/term_pane.rs` per §3.5, wired to selection. Preview only — no input yet.
Unit-test the renderer by feeding canned escape sequences into a `Term` and
asserting buffer cells (colors, wide chars, alt-screen), in the spirit of the
existing `parse_session_line` tests.

### Task 3 — focus model + input path
§3.3: `Focus`, key encoder, jump-in/out, cursor. Encoder unit tests: arrows
in normal vs application-cursor mode, modified keys, Enter/Esc/Backspace/Tab,
paste. Manual pass of the Task 0 checklist items 1–2 inside the real TUI.

### Task 4 — delete tmux + docs + guard
Remove `session/tmux.rs`, the `main.rs` bootstrap/placeholder/retarget code,
`nested_attach_command`, tmux version-check; update `AGENTS.md`
(architecture diagram, keybind table — `Ctrl-h`/`Ctrl-l` pane navigation
replaces `<prefix> o`, runtime deps line: drop `tmux`, add nothing), `README`/docs mentions; rewrite
`scripts/test_lifecycle.sh` against the new backend; add the quit guard
(§3.2) with a line in the status bar while agents run. Update the memory of
record: v1 has no persistence — quitting kills agents.

### Task 5 — post-phase backlog (do not build now)
Scrollback UI → mouse forwarding → copy/paste → daemon persistence split
(the herdr `server/`/`client/`/`persist/` layout in `~/projects/herdr` is
the reference; `shpool` is the buy-not-build alternative). Ordering TBD with
Phase 5b's deferred UX items.

---

## 5. Testing & validation

- Every task lands with its unit tests (mock session manager keeps the
  existing lifecycle suite alive; renderer and encoder get new direct tests).
- The per-task manual gate is always **real Claude Code**, not mock — the
  project's own recorded lesson from Phase 5.
- End of phase: full manual run — `n` root, run `claude` inside it, `s` a
  child, watch both statuses, jump in, approve a real permission prompt,
  jump out, `d` the child, quit-guard prompt, quit.

## 6. Risks (from the findings doc, now with owners in-plan)

| Risk | Where handled |
|---|---|
| Query traffic Claude Code needs but alacritty_terminal won't answer (e.g. XTGETTCAP → terminfo fallback) | Task 0 item 1–2; TERM decision §2 |
| Key-encoder gaps (modifiers, app-cursor mode) | Task 3 tests + manual gate |
| Wide-char/emoji column shear | Task 0 item 4; Task 2 tests |
| N chatty agents burn CPU in parsers | Task 0 item 6; scrollback cap §2; only-selected rendering §3.5 |
| Agents die with the TUI (v1 regression) | Quit guard, Task 4; daemon split deferred to Task 5 backlog |
| 0.26 API drift vs this plan's assumptions | Task 0 item 5 — plan text yields to spike findings |
