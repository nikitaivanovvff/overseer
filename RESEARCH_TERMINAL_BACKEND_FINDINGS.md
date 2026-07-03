# Terminal backend research — findings & recommendation

Answers `RESEARCH_TERMINAL_BACKEND.md`. Researched 2026-07-02 against the live
codebase (`session/tmux.rs`, `main.rs` bootstrap, `PHASE5.md`), the local herdr
clone, and current upstream state of Zellij and the Rust terminal-emulation
ecosystem.

**Decision inputs confirmed with the maintainer before writing this:**
persistence may slip past v1 (a daemon split is acceptable later); the choice
is on long-term merits (Phase 5/5c sunk cost is irrelevant); tmux-as-invisible-
backend is neither required nor forbidden — judged on merits; v1 pane bar is
keys-in / faithful-rendering-out, mouse + copy/paste later.

---

## Recommendation

**Build the custom PTY layer (option 3), on `alacritty_terminal` — not
libghostty-vt, not vt100.** Drop tmux entirely. Accept that persistence
arrives in a later phase as an Overseer daemon in front of the same layer.

The one-line justification: the reason herdr's approach looked unaffordable —
4,400+ lines of hand-rolled emulation plus ~2,200 more lines answering
terminal queries — is a cost `alacritty_terminal` has already paid. It is the
exact emulation core of the Alacritty terminal and of Zed's embedded terminal:
it owns the PTY (its `tty` module spawns on unix + windows, no `portable-pty`
needed), parses everything, maintains the full grid with colors/attributes/
alt-screen/scroll-regions/wide-chars, **and generates the replies to terminal
queries itself** (device attributes, DSR/cursor-position, kitty-keyboard mode
reports) — surfaced as `Event::PtyWrite` payloads you simply write back to the
PTY. "Will Claude Code's TUI render faithfully?" reduces to "does Claude Code
work inside Alacritty/Zed?" — which it demonstrably does, for every TUI.

What Overseer actually has to write:

| Piece | Size (est.) | Notes |
|---|---|---|
| PTY session type (spawn agent cmd w/ env, feed parser, handle `PtyWrite`) | ~200–300 lines | replaces `TmuxClient::launch` |
| Grid → ratatui `Buffer` renderer | ~300–500 lines | iterate renderable cells, map colors/flags; wide-char cells are the fiddly part |
| Input encoder (crossterm key event → escape bytes) | ~300–400 lines | arrows/modifiers/paste; crib from Zed's `terminal` crate — this is the one piece alacritty_terminal leaves to the app |
| Resize plumbing (rect change → PTY winsize) | small | |
| **Total v1** | **~1–1.5k lines** | vs herdr's ~8k `pane/` module — because herdr predates usable emulation crates and hand-rolled everything |

What this buys, per the criteria:

1. **Newcomer-friendliness — fully solved.** One process, one keymap, all
   ours. Jump-in/out is a focus flag in our own event loop, not a tmux client
   switch. No prefix keys exist anywhere in the product.
2. **Faithful rendering — inherited, not built.** Alacritty's emulator, used
   by Zed in production for exactly this embedding shape.
3. **Persistence — deferred, with a real path** (below). This is the honest
   regression: in v1, quitting/crashing the Overseer process kills agents,
   which today's tmux backend survives. Deemed acceptable past v1.
4. **Maintenance — bounded.** ~1–1.5k lines of ours; the hard 100k-line part
   is an actively maintained Apache-2.0 crate on crates.io with Alacritty and
   Zed as its own consumers. Compare: today's tmux backend is only ~570 lines,
   but its *implicit* surface (nesting, `$TMUX`, remain-on-exit, respawn
   self-heal, client-tty targeting, version gates) is what produced this
   project's hardest bugs.
5. **Reliability — fewer moving parts.** No external server process whose
   implicit state we rent. Bugs become ordinary in-process Rust bugs with
   `cargo test`able parsers, not cross-process tmux behaviors only a live
   spike catches.
6. **Cross-platform — fine.** alacritty_terminal supports macOS/Linux (and
   Windows, if that ever matters — tmux never would).

Deleted outright: the tmux bootstrap/re-exec in `main.rs`, the placeholder
session, the nested-attach `$TMUX` dance, `split_pane`/`pane_tty`/
`switch_client_on`/`respawn_pane` and the remain-on-exit self-heal — i.e. the
exact code that generated Phase 5's bug list. `registry`, `ipc`, `status`
(push-based, backend-agnostic per AGENTS.md) carry over unchanged.

---

## Why not the alternatives

### tmux status quo — the ceiling on hiding it is real but below "invisible"

Most leakage *is* fixable: status bar (already disabled server-wide), private
`-L` server (done), and the return-from-pane key could move off `<prefix> o`
to a root-table binding on a key agents never need (e.g. `C-\`), making prefix
knowledge unnecessary in the happy path. But three leaks are structural:

- **The user's terminal is a tmux client.** Copy-mode, mouse-mode, resize
  semantics, and any tmux server hiccup are tmux's, not ours. Every UX idea
  must be expressible in tmux primitives — Phase 5c was a tour of what that
  costs (size-mismatch redraw corruption, nested-attach refusal, status-bar
  bleed).
- **Any root-table "return" key is stolen from the agent** underneath — there
  is no key that is both always available and never wanted by some TUI.
- **User-already-in-tmux nesting** can be softened (we unset `$TMUX`) but the
  visual nesting of their client around ours is theirs, not ours, to fix.

Verdict: workable, never fully ours. If the custom route's spike fails, the
fallback is "stay on tmux + move the return key to a root-table binding," not
Zellij.

### Zellij — better scripting than tmux now, worse fit for our core mechanic

Current state (verified against zellij.dev docs and the 0.44 release notes):
headless background sessions (`attach --create-background`), pane-ID-addressed
`send-keys`/`paste`, `dump-screen`, `list-panes --json`, and a real-time
`subscribe` stream. Genuinely good **headless automation** — but Overseer
doesn't need output-scraping automation (status is push via our own IPC), and:

- **No library crate.** Same shell-out-to-binary model as tmux, except tmux is
  preinstalled ~everywhere and Zellij isn't.
- **No `switch-client` equivalent.** The load-bearing trick of our current UI
  — retargeting one specific embedded client to another session — has no
  Zellij CLI primitive. Session switching from inside a client goes through
  the session-manager plugin; driving it programmatically means shipping our
  own WASM plugin and messaging it via `zellij pipe`. Strictly more machinery
  than the tmux one-liner we're trying to escape.
- **Keybinding model doesn't solve our problem.** Its discoverable modal UI
  binds many bare `Ctrl-*` keys by default — *worse* collisions with agent
  TUIs than tmux's single prefix; we'd ship a locked-down config and be back
  to hiding a multiplexer.
- **Younger/faster-moving:** 0.44 notes say existing sessions don't survive
  upgrades (protocol compat is future work). License MIT — fine.

Verdict: rejected. It answers a question we don't have (scripted output
observation) and lacks the primitive we actually use.

### libghostty-vt — real now, but the wrong half is still on us

Since herdr vendored it by hand, official Rust bindings shipped
(`libghostty-vt` 0.2.0, June 2026, MIT/Apache-2.0, from Ghostty maintainers).
But: API explicitly unstable; types are `!Send`/`!Sync`; and its scope is
parsing + state — **the embedder still answers terminal queries**. That's
empirically what herdr's extra ~2,200 lines (`osc.rs` 1,720, `xtgettcap.rs`
343, `kitty_keyboard.rs` 125) exist for: watching the raw stream and
synthesizing DA/XTGETTCAP/kitty replies. alacritty_terminal does that part
internally. Worth revisiting in a year; not today's choice.

### vt100 / tui-term — insufficient for interactive full-screen agents

`tui-term` is the obvious-looking ratatui widget, but its only backend is
`vt100`: parse-only (no query replies — full-screen TUIs that probe the
terminal hang or degrade), sporadically maintained, and turborepo had to fork
it for render performance. Fine for read-only previews; not for "jump into
Claude Code and it behaves like a real terminal."

### Hybrids — only the persistence hybrid survives scrutiny

"tmux headless for persistence + our rendering" sounds appealing but the
interactive path is a trap: control-mode `%output` hands you raw bytes — you
need an emulator anyway (so tmux adds nothing but a dependency), and
`capture-pane -e` polling is a lossy snapshot (cursor, latency, no mouse) —
fine for previews, not for jump-in. wezterm's mux server is a heavier
dependency than the problem. **The keeper:** persistence later does not have
to mean "write our own daemon protocol from scratch" — options, in order:
(a) Overseer daemon owning the PTY+emulator pairs, thin TUI client over the
existing tokio unix-socket IPC (reattach = serialize grid + stream deltas;
herdr's `server/`/`client/`/`persist/` split is a working reference
implementation sitting in `~/projects/herdr`); (b) park sessions under
`shpool` (Google's Rust session-persistence daemon — persistence without
multiplexing) if (a) proves heavy.

---

## Migration path (minimum viable)

1. **Task 0 — throwaway spike, same discipline as PHASE5.md §2.** One binary:
   spawn `claude` in an `alacritty_terminal` PTY, render its grid into a
   right-hand ratatui rect beside a dummy sidebar, forward keys when focused.
   Must prove: faithful full-screen rendering (colors, alt-screen, redraw),
   typing/approving a permission prompt works, resize reflows, Esc/arrows/
   modifiers arrive correctly. If this fails, stop — fallback is tmux +
   root-table return key.
2. Introduce `session/pty.rs` implementing today's session-boundary verbs
   (launch w/ env, kill, exists, "attach" becomes render+focus) behind a trait
   so `registry`/`spawn`/`ipc` don't change; `TmuxClient` and the `main.rs`
   bootstrap/placeholder/nested-attach code are deleted rather than ported.
3. Preview and jump-in collapse into one path: always render the selected
   agent's grid; jump-in = route input to its PTY; `Esc`-or-similar (ours to
   choose, at last) returns focus to the tree.
4. Later phase: daemon split for persistence (path (a) above).

## Riskiest unknowns to spike (in Task 0)

1. **Query traffic under real Claude Code** — confirm alacritty_terminal's
   replies satisfy it (it lacks XTGETTCAP; apps fall back to `$TERM`/terminfo
   — set `TERM` to a value whose terminfo is universally present, e.g.
   `xterm-256color`). Watch for kitty-keyboard negotiation.
2. **Input encoding completeness** — the app-side key→bytes encoder is the
   one genuinely new component; verify modifier combos and paste against
   Claude Code's prompt before generalizing.
3. **Wide-char/emoji cells in the grid→ratatui mapping** — agent output is
   full of them; off-by-one column bugs are the classic failure.
4. **N background emulators** — all agents' parsers must consume PTY output
   continuously (state must stay current for instant preview); measure CPU
   with ~6 chatty agents, throttle if needed.
5. **v1 lifecycle regression** — agents die with the process now; decide how
   loudly to warn on quit until the daemon phase lands.

## Sources

- [Zellij: programmatic control](https://zellij.dev/documentation/programmatic-control.html) · [CLI actions](https://zellij.dev/documentation/cli-actions) · [0.44 release notes](https://zellij.dev/news/remote-sessions-windows-cli/) · [nesting discussion](https://github.com/zellij-org/zellij/discussions/2448)
- [alacritty_terminal on crates.io](https://crates.io/crates/alacritty_terminal) · [Zed's embedding of it](https://github.com/zed-industries/zed/blob/main/crates/terminal/src/terminal.rs)
- [libghostty-vt Rust crate](https://docs.rs/libghostty-vt) · [ghostling reference terminal](https://github.com/ghostty-org/ghostling)
- [tui-term](https://github.com/a-kenji/tui-term) · [turborepo's vt100 perf fork PR](https://github.com/vercel/turborepo/pull/9123)
- Local: `~/projects/herdr` (vendored libghostty-vt + portable-pty; `src/pane/` ≈ 7.8k lines incl. hand-rolled query replies; `server/`+`client/`+`persist/` = working daemon-split reference)
