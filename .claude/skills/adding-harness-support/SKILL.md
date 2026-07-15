---
name: adding-harness-support
description: Recipe for adding a new AI coding harness adapter to Overseer beyond claude/opencode. Use when asked to integrate a new agent CLI/TUI as an Overseer adapter.
---

Overseer launches an agent harness in a PTY and needs it to report its own
status back (`spawning → running/idle/blocked → done/error`) without polling.
"Supporting a harness" means writing one `AgentAdapter` implementation plus
its install-time content files. `src/agent/adapters/claude.rs` and
`opencode.rs` are the reference implementations — read whichever is closest
in shape to the new harness before writing anything.

## The five deliverables

Every adapter must deliver all five, all already abstracted by the
`AgentAdapter` trait (`src/agent/adapters/mod.rs`):

1. **Capabilities** (`capabilities`) — declare lifecycle, permission requests,
   provider limits, and context usage as `Supported`, `Unsupported { reason }`,
   or `Experimental { note }`. Never use a bool or claim `Supported` before a
   live probe demonstrates the structured signal.
2. **Launch** (`spawn_command` + `env_inject`) — start the harness in a PTY
   with the task as its initial prompt, staying interactive (never a
   one-shot/print mode); inject identity env (`identity_env`) +
   `OVERSEER_TASK` when the task is non-empty (never inject it for an empty
   task — that's a root's bare-shell case, no task exists).
3. **Status wiring** (`install_files`, install-time) — a hook/plugin/extension
   file that pushes `overseer status <s>` on the harness's own lifecycle
   events. Must be a no-op outside Overseer (guard on `$OVERSEER_AGENT_ID`
   being set) and must call the absolute Overseer binary path, embedded at
   generation time (`std::env::current_exe()`) — the harness's subprocess
   won't inherit the shell `$PATH` entry that made `overseer` findable in the
   terminal you're reading this in.
4. **Role instructions** (`install_files`, install-time) — root/child
   behavior docs the harness will actually load. Content: spawn/monitor/drop
   for roots (bless cross-harness spawning: `--adapter claude|opencode|...`
   — an agent doesn't have to run its own harness for its children), worktree
   isolation + explicit `overseer status done --message …` for children
   (**never** inferred from the harness going quiet or the session ending).
5. **Uninstall** — `install_files()`'s `MergeStrategy` per file determines
   this automatically: `Overwrite` files get deleted outright, `JsonMerge`/
   `JsonArrayMerge` files get exactly Overseer's entries stripped back out,
   leaving the user's own content untouched. `legacy_paths()` covers a rename
   of the install layout itself (see `ClaudeAdapter::legacy_paths` for why:
   a stale copy of a superseded layout must not sit alongside the new one).

Status vocabulary every harness must map onto (semantics fixed in AGENTS.md
— do not invent new ones):

| Status | Meaning | Reached via |
|---|---|---|
| `spawning` | registered, session launching | set by Overseer itself, never a hook push |
| `running` | actively working | hook/plugin push on a "started responding" event |
| `idle` | finished responding, awaiting more input | hook/plugin push on a "turn ended" event |
| `blocked` | needs a human — a permission/approval prompt is pending | hook/plugin push, **only if the harness actually has this concept** — see the caveat below |
| `done` | explicitly declared complete | the agent's own `overseer status done`, never inferred |
| `error` | process exited unexpectedly | the exit watcher (session manager), never a hook push |

**If the harness has no permission-prompt concept**, do not fake `blocked` by
guessing at some other event. Document the gap plainly in the harness's own
root instructions doc so the root agent knows not to expect it.

## Files to touch

- `src/agent/adapters/<name>.rs` — the `AgentAdapter` impl.
- `src/agent/adapters/<name>_root.md` / `<name>_child.md` (or reuse the
  `.md`/`SKILL.md` convention if the harness has one) — role instructions,
  `include_str!`'d into the adapter, mirroring `claude.rs`'s
  `ROOT_SKILL_CONTENT`/`CHILD_SKILL_CONTENT` pattern.
- `src/agent/adapters/mod.rs` — add `pub mod <name>;` and a match arm in
  `adapter_for`.
- `src/config/mod.rs`'s `default_adapters()` — a built-in `AdapterConfig`
  entry so `config.adapters["<name>"]` resolves out of the box.
- `src/settings.rs` — only if the harness's config file needs a merge shape
  `JsonMerge`/`JsonArrayMerge` doesn't already cover (both are generic enough
  for "hooks-object-of-arrays with an `_overseer` sentinel" and "flat string
  array" respectively — check those first).
- `AGENTS.md` — the adapter/config sections; see PHASE5B.md/HARNESSES.md's
  own AGENTS.md diffs for the shape (a per-harness mapping-table style
  section, not prose).

## Verification checklist (the house rule, generalized)

**Run the real harness under Overseer before calling anything done** — a
config schema read from docs is a hypothesis, not a fact, and harnesses move
fast enough that a 2026-vintage doc can already be stale by the time you read
it. Docs (even this skill) are a starting point; the installed binary is the
source of truth. Concretely, for a fresh harness integration:

1. **Launch invocation**: confirm the exact flag/positional-arg form that
   takes an initial prompt *and stays in interactive TUI mode* — many
   harnesses have both an interactive default and a separate one-shot/print
   mode (`opencode run` vs. the default `opencode` command). Picking the wrong
   one silently turns every spawned agent into a fire-and-forget script instead
   of a steerable session.
2. **Plugin/extension loading**: write a minimal probe (log to a scratch
   file: "loaded", plus every event name as it fires) and confirm it actually
   loads, can read `process.env`, and can exec a subprocess — from the
   harness's own real config/extension directory or invocation flag, not a
   guess from documentation. If the harness supports loading a hook file by
   direct path at invocation time, prefer that over registering into the
   harness's own settings file — it keeps `overseer install`/`--uninstall` to
   "write/delete one file," no settings-file mutation needed at all.
3. **Event names for the five moments**: session start, agent starts
   working, agent finishes responding, permission request (if any), session
   end. Get these from the harness's own installed type definitions/source if
   readable (`node_modules/<pkg>/dist/*.d.ts`, a package's own TS source) —
   this is strictly more reliable than prose docs, which describe intent, not
   the exact runtime event/field names. If the harness ships a discriminated
   event union (`event.type === "..."`) or a generic `event`-bus-plus-
    dedicated-hooks split (OpenCode 1.17.20's permission event is on the
    generic bus even though a dedicated hook remains in its declarations), that split usually
   isn't obvious from docs alone — trace it in the types.
4. **A real end-to-end spawn**: install for real, spawn a child through the
   actual daemon (not `--mock`) with a trivial, cheap prompt, and watch
   `overseer list` show `spawning → running → idle` live. Then push
   `overseer status done` from that agent's identity context and confirm it
   lands. This is the gate that catches a subtly wrong event mapping that
   type-reading alone can miss (an event that's *documented* to fire but
   doesn't, in practice, reach a same-process plugin — verified true for
   opencode's `permission.updated` bus event during this work).
5. **Drop a live session** (Phase 6 lesson): confirm `overseer drop <id>`
   actually terminates the harness's process — some agents ignore SIGHUP
   or fork long-lived children that outlive the parent. Don't assume the
   existing `SessionManager::kill` behavior generalizes without checking.
6. **Uninstall round-trip**: install, then uninstall, then diff the touched
   config file(s) against their pre-install state — a `JsonMerge`/
   `JsonArrayMerge` file must come back byte-for-byte identical (modulo
   pretty-printing) to what it was before, proving Overseer only ever
   touched its own entries.

## Test checklist (mirror `claude.rs`'s shape)

- `install_files()` returns the expected files, in order, with the expected
  `MergeStrategy` per file.
- `capabilities()` has a pure matrix test; every supported handler is present
  in the installed artifact, and captured sanitized payload fixtures exercise
  the event normalizer.
- The generated hook/plugin/extension content: guards on `$OVERSEER_AGENT_ID`
  being unset (a no-op), embeds the absolute Overseer binary path, maps every
  status-relevant event to the right push (and — importantly — a test
  asserting it does **not** fabricate an event/status the harness doesn't
   actually support).
- `spawn_command`: uses `ctx.command`/`extra_args` correctly; appends the
  task via whichever mechanism was verified in Task 2 above (positional arg
  vs. a named flag) when non-empty; appends nothing extra for an empty task
  (the root case).
- `env_inject`: identity vars present; `OVERSEER_TASK` present only when
  `ctx.task` is non-empty.
- Role instruction content: contains the role guard where applicable,
  documents `overseer status done` for the child doc, blesses cross-harness
  `--adapter` spawning in the root doc, and documents any `blocked`-support
  caveat plainly if the harness lacks a permission-prompt concept.
- If a new `MergeStrategy` variant was needed: pure merge/remove function
  tests in `settings.rs` (idempotent merge, dedup, user-content-preserving
  removal, no-op on an absent file) — mirror `merge_json_array`/
  `remove_json_array`'s test shape.

## Out of scope for this skill

- Auto-detecting which harnesses are installed on a machine — `overseer
  install <name>` is always an explicit, user-initiated step.
- Per-harness context-percentage parity. Keep it machine-readable only when
  the harness authoritatively supplies both usage and its active window size;
  never divide transcript tokens by a hardcoded guessed constant. Context is
  intentionally not rendered in the TUI.
