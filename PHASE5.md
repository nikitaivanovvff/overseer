# Phase 5 — Pane (jump-in + live preview)

> Implementation plan for the next model. **Scope: the pane only.** The other four
> Phase-5 items in `TASKS.md` (fuzzy search, config loading, generic adapter, focus-keybind
> revisit) are deferred to Phase 5b — see the last section. Read `AGENTS.md` first; it is the
> source of truth for constraints. Nothing here overrides its "What to Avoid" list.

---

## 1. Goal

Make the right-hand panel real. Two behaviors:

1. **Live preview (browsing).** While the cursor moves over the agent tree, the right panel
   shows a read-only snapshot of that agent's terminal (its tmux pane), refreshed on a throttle.
   Replaces the current `"pane embedding — phase 5"` placeholder in `render_pane_body`.
2. **Jump-in (Enter).** Pressing `Enter`/`o` on a selected agent drops the user into that
   agent's **real** tmux session — a fully interactive terminal where they can type, approve
   permission prompts, and steer the agent. A single keystroke returns them to the Overseer TUI.

This is the "hit Enter → land in the running agent → interact/approve → come back" flow.

### Non-goals for this phase
- No in-process terminal emulator (no `tui-term`/`vt100`/PTY). tmux is the terminal; we do not
  re-implement one. The preview is a text snapshot; real interaction is a tmux client switch.
- No split-pane "IDE" compositing (Overseer + live agent pane side by side). Noted as a future
  enhancement in §9; `switch-client` is simpler and matches the intended flow.
- None of the four deferred UX items.

---

## 2. Chosen approach & the one real unknown

**Approach: capture-pane preview + `switch-client` jump-in, on a private tmux server.**

- Preview = `tmux capture-pane -t <session> -p` rendered into the ratatui rect (read-only).
- Jump-in = `tmux switch-client -t <session>` — the user's tmux client now displays the agent's
  real session. Return = a key we bind on our tmux server that switches back to the Overseer session.

For `switch-client` to work, **Overseer's own TUI must run inside tmux**. Today it is a standalone
alt-screen app (`run_tui` in `main.rs`), so this is the main new moving part.

**The one genuinely uncertain mechanic** is: host-in-tmux + jump-in + return, working cleanly
(ratatui repaints correctly on return, the return key is reliable, and none of it clobbers the
user's own tmux). That is exactly what `TASKS.md`'s "spike" was for. So **Task 0 is a throwaway
proof of just that mechanic** (§5). Everything after Task 0 assumes it worked and builds the real
thing. If Task 0 surfaces a blocker, stop and raise it before building further.

### Decision: use a **private tmux server** (`tmux -L overseer …`)
All Overseer tmux calls (existing and new) target a dedicated server via `-L overseer` instead of
the user's default server. This buys three things at once:

- **Isolation** — Overseer's TUI session and every agent session live off the user's normal tmux;
  nothing pollutes their session list.
- **Free rein over key bindings** — tmux bindings are server-global, so a "return to Overseer" key
  can be bound on our server without touching the user's config.
- **Predictable naming** — the Overseer TUI session has a known name to switch back to.

Tradeoff to accept: agents are no longer visible in the user's default `tmux ls`; to poke one
outside the TUI the user runs `tmux -L overseer attach -t overseer-<id>`. This matches
"Overseer launches the session and gets out of the way" and is cleaner than the alternative.

> Alternative considered (not chosen): stay on the default server, capture the current session
> name at startup as the return target, and bind the return key in the tmux *prefix* table to
> avoid stealing a root-table key from agents. Rejected because a prefix binding on the shared
> server still mutates the user's tmux, and their agents intermingle with their own sessions.
> If Task 0 shows `-L` causes trouble, this is the fallback — flag it, don't silently switch.

---

## 3. Architecture changes

### 3.1 `session/tmux.rs` — the tmux boundary (all changes live here)
`AGENTS.md`: *"`TmuxClient` is the only tmux boundary. No raw `Command::new("tmux")` outside
`session/tmux.rs`."* Honor it — every new tmux verb below is a `TmuxClient` method.

- **Route through the private server.** Introduce one helper that every method uses to build its
  base command, e.g. `fn tmux(&self) -> Command` returning `Command::new("tmux")` pre-loaded with
  `["-L", "overseer"]`. Convert `list_sessions`, `new_session`, `kill_session`, `session_exists`,
  `launch`, and `check_min_version` to build from it. The server label can be a `const`
  (`const SERVER: &str = "overseer";`). Keep `dry_run` gating exactly as-is.
- **`capture_pane(&self, session: &str) -> Result<String>`** — runs
  `capture-pane -t <session> -p` (consider `-e` later for color; start plain). In `dry_run`,
  return a canned string like `"(preview unavailable in mock mode)"` so `--mock` still renders.
  Non-zero exit (session gone) → return `Ok(String::new())`, not an error (the watcher handles
  reaping; preview just shows blank).
- **`switch_client(&self, session: &str) -> Result<()>`** — `switch-client -t <session>`. `dry_run`
  → `Ok(())`.
- **`bind_return_key(&self, home_session: &str) -> Result<()>`** — sets up the return binding on the
  private server, e.g. `bind-key -T prefix o switch-client -t <home_session>` (prefix-based so it
  does not steal a bare key from the agent). Called once at TUI startup. `dry_run` → `Ok(())`.
  *Task 0 decides the exact key/table; wire the decision here.*
- **`ensure_tui_session` / bootstrap** — see §3.2; the actual "am I in tmux, if not create+attach"
  logic may live in `main.rs` since it re-execs the process, but any `tmux` invocation it needs
  (`new-session`, `attach-session`, `display-message -p '#S'`) must be `TmuxClient` methods.

Add unit tests mirroring the existing style: `capture_pane`/`switch_client`/`bind_return_key` are
no-ops under `dry_run()`; any new pure parse/format helper gets a direct test.

### 3.2 `main.rs` — host the TUI inside tmux
`run_tui` currently sets raw mode + alt screen directly. New startup sequence (before that):

1. Determine whether we're already inside our tmux server. If `$TMUX` is set **and** we're on the
   `overseer` server, run in place. Otherwise **bootstrap**: create session `overseer` on the
   private server running this same binary (`tmux -L overseer new-session -s overseer -- <self>`)
   and `attach` to it, then exit the outer process. (Re-exec pattern — the attached copy is the one
   that runs the ratatui loop.) Guard against infinite re-exec with an env flag, e.g.
   `OVERSEER_TUI_HOSTED=1`, set on the inner launch and checked on entry.
2. Once hosted, resolve the **home session name** (the Overseer session, `"overseer"`) and call
   `tmux.bind_return_key(home)` so the return key works from inside any agent session.
3. Proceed into the existing raw-mode/alt-screen ratatui setup unchanged.

Keep `--mock` fully offline: in mock mode, skip the tmux bootstrap and the bind entirely (mock uses
a `dry_run` `TmuxClient` already). `--mock` must never require or create a real tmux server.

### 3.3 `app.rs` — preview state
Add to `App`:
- `pub pane_preview: Option<String>` — last captured snapshot for the selected agent.
- `pub preview_for: Option<AgentId>` — which agent the snapshot belongs to (so a cursor move
  invalidates it immediately instead of showing a stale neighbor's output).
- a throttle marker (reuse `tick`: only re-capture every N ticks, e.g. every 4 ≈ 400ms).

`Focus` today is `{ Tree, Pane }` with `Pane` being the in-ratatui placeholder-focus. Under the new
model "focus the pane" means "you left ratatui and are in the agent's tmux session," so there is no
in-app `Pane` focus state to hold. **Simplify `Focus` to just tree browsing** (or drop the enum and
the `Tab`/focus-toggle path). This also discharges the deferred "revisit focus-toggle keybind" item
for the pane's sake — do the minimal removal here; the broader keybind pass stays in 5b.

### 3.4 `main.rs::run_app` — capture + jump-in in the event loop
- **Capture (throttled, off the draw path).** Before `terminal.draw(...)`, if the throttle fired and
  a node is selected, call `capture_pane` on the blocking thread and store into
  `app.pane_preview` / `app.preview_for`. Only the *selected* agent is captured — never the whole
  tree. Skip when mock/dry-run returns the canned string is fine (still store it).
- **Jump-in.** Replace the current `Enter/o ⇒ app.focus = Focus::Pane` arm with: resolve the selected
  agent's session name via `agent::spawn::tmux_session_name(&id)` and call
  `tmux.switch_client(&session)`. On error (session gone), set `status_message`. After the switch the
  ratatui loop keeps running in the background; on return it repaints on the next tick.
- Remove the `Focus::Pane` Esc arm (no longer reachable).

### 3.5 `ui/mod.rs` — render the preview
- `render_pane_body`: replace the placeholder lines with the captured text. Take `pane_preview:
  Option<&str>` (thread it through `render` alongside the tree). Split on `\n`, clip to the rect
  height/width, render as `Paragraph` inside the existing `" PANE "` block. If `None`/empty, show a
  muted `"  no output yet"`.
- `render_pane_header`: keep repo/branch; optionally append a hint like `↵ jump in`.
- Status bar hints: drop the `Esc → agents` (Pane-focus) branch; add `↵ jump in` to the tree hints.
  Add the return-key hint (`<prefix> o → overseer`) to a spot the user sees *before* jumping in
  (pane header or status bar), since once they're in the agent session the ratatui hints are gone.

`ui/mod.rs` stays render-only (`AGENTS.md`) — no capture calls, no tmux, no business logic there.

---

## 4. `render`/signature threading
`ui::render` currently takes `(frame, &Focus, tree, tick, prompt)`. Add the preview:
`ui::render(frame, tree, tick, prompt, pane_preview: Option<&str>)` (and drop `&Focus` if `Focus`
is removed). Update the single call site in `run_app`.

---

## 5. Task breakdown (in order)

**Task 0 — Fix the `OVERSEER_REPO` bug (do this first).**
`src/agent/adapters/claude.rs::env_inject` sets `OVERSEER_REPO` to `ctx.task` (the task string)
instead of the repository name — a copy/paste slip vs the env-var table in `AGENTS.md` §Agent
Awareness. Proper fix: `LaunchContext` has no repo field today, so add `pub repo: String` to
`LaunchContext` (`src/agent/adapters/mod.rs`), populate it from `req.repo` where the context is
built in `src/agent/spawn.rs::spawn_agent` (and any other `LaunchContext` construction, incl. test
fixtures), and change `env_inject` to insert `ctx.repo.clone()` for `OVERSEER_REPO`. Add/adjust a
unit test asserting `OVERSEER_REPO` is the repo name, not the task. Land this on its own before
touching the pane so it stays an isolated, reviewable fix.

**Task 1 — Spike the jump-in/return mechanic (throwaway).**
Smallest possible proof, on the private `-L overseer` server, by hand or a scratch binary:
create an `overseer` session running a placeholder, create an `agent` session running a shell,
from the overseer session `switch-client -t agent`, confirm you land in an interactive shell,
press the bound return key, confirm you're back and the placeholder repainted. Nail down:
the exact return key + table, that `-L overseer` isolates cleanly, and that a ratatui alt-screen
app repaints correctly after being switched away and back. **Throw the spike away.** If any of
this doesn't hold, stop and report before Task 2.

**Task 2 — Private server + new tmux verbs.** §3.1. Route all tmux calls through `-L overseer`;
add `capture_pane`, `switch_client`, `bind_return_key`; unit tests for dry-run no-ops. Existing
tmux tests must still pass. Update `scripts/test_lifecycle.sh` if the `-L` label breaks its setup
(it uses `TMUX_TMPDIR` isolation — with `-L` it should still be isolated; verify).

**Task 3 — Host the TUI in tmux.** §3.2. Bootstrap/re-exec into the `overseer` session; the
`OVERSEER_TUI_HOSTED` re-exec guard; call `bind_return_key` at startup; `--mock` skips all of it.
Manual check: `overseer` from a bare shell lands you in a tmux-hosted TUI; `overseer --mock` from a
bare shell still runs with no tmux server created.

**Task 4 — Preview state + capture loop.** §3.3 + §3.4 (capture half). Throttled capture of the
selected agent into `App`; invalidate on cursor move.

**Task 5 — Render the preview.** §3.5 + §4. Wire `pane_preview` through `render`; replace the
placeholder; update hints. Simplify/remove `Focus::Pane`.

**Task 6 — Jump-in on Enter.** §3.4 (jump-in half). `Enter`/`o` ⇒ `switch_client`; error handling;
remove the old focus arm.

**Task 7 — e2e.** §7. Extend `scripts/test_lifecycle.sh` with a "Part C — pane" that drives the
hosted TUI, sends `Enter`, asserts the client switched to the agent session, sends the return key,
asserts it switched back.

**Task 8 — Review + memory.** Run a medium code-review pass (this project's convention — see the
Phase 4 commit). Fix what it finds. Update the memory files (`phase-progress.md`, and `MEMORY.md`
index if needed) and `TASKS.md` checkboxes for the pane item.

---

## 6. Files touched (quick map)
| File | Change |
|------|--------|
| `src/agent/adapters/mod.rs` | Task 0: add `repo: String` to `LaunchContext` |
| `src/agent/adapters/claude.rs` | Task 0: `env_inject` uses `ctx.repo` for `OVERSEER_REPO`; test |
| `src/agent/spawn.rs` | Task 0: populate `LaunchContext.repo` from `req.repo` (+ fixtures) |
| `src/session/tmux.rs` | `-L overseer` routing; `capture_pane`, `switch_client`, `bind_return_key`; tests |
| `src/main.rs` | tmux hosting/bootstrap + re-exec guard; capture throttle + jump-in in `run_app`; render call site |
| `src/app.rs` | `pane_preview`, `preview_for`, throttle; simplify `Focus` |
| `src/ui/mod.rs` | render preview in `render_pane_body`; hints; `render` signature |
| `scripts/test_lifecycle.sh` | Part C — pane jump-in/return e2e |
| `AGENTS.md` | (optional) tighten the "right panel" description now that it's real |
| memory + `TASKS.md` | progress + checkbox updates (Task 8) |

---

## 7. Testing strategy
- **Unit (fast, no tmux):** dry-run no-ops for the three new verbs; any pure parse/format helper;
  preview-invalidation logic if it's extractable as a pure function. Keep the `dry_run` pattern —
  do not introduce trait abstractions for tmux (`AGENTS.md` "How it is").
- **e2e (`test_lifecycle.sh`, real tmux):** the existing Part A (CLI) and Part B (TUI send-keys)
  stay green. New Part C proves jump-in/return against a real private server + stub `claude`.
- Manual smoke: `overseer` in a bare shell → hosted TUI → spawn a root → arrow onto it → preview
  shows the stub's output → Enter → interactive session → return key → back in the TUI.

---

## 8. Risks & watch-items
- **Re-exec loop.** The tmux bootstrap re-launches the binary; the `OVERSEER_TUI_HOSTED` guard must
  be set on the inner launch and checked first, or `overseer` forks forever. Test the guard early.
- **Return key reliability / conflict.** A root-table (`-n`) binding steals the key from the agent's
  own terminal; use the prefix table. Task 0 locks the exact key.
- **Capture cost.** Only ever capture the *selected* agent, throttled. Never capture the whole tree
  and never on every 100ms draw.
- **Preview staleness.** Invalidate `pane_preview` the instant the cursor moves (`preview_for` guard)
  so the panel never shows the previous agent's output under the new agent's header.
- **Mock stays offline.** `--mock` must not create a tmux server, bootstrap, or bind keys.

---

## 9. Deferred (Phase 5b — not in this plan)
- Fuzzy agent search (`/` to filter the tree, Esc to cancel).
- Config file loading (`~/.config/overseer/config.toml`) — schema already in `AGENTS.md` §Config;
  needs a `toml` dep (none yet). Precedence: CLI flag > config > default.
- Generic agent adapter (raw shell command + env vars) — a second `AgentAdapter` impl + entry in
  `adapter_for`.
- Broader focus-keybind pass (`Tab`, F2 placeholder) beyond the minimal `Focus::Pane` removal done here.
- **Future enhancement:** split-pane "IDE" mode (Overseer + a live agent tmux pane side by side via
  `join-pane`), if the `switch-client` flow proves too jarring.

## 10. Known cleanup
- `OVERSEER_REPO` bug — promoted to **Task 0** (§5); fix it first, before any pane work.
