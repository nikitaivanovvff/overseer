---
name: overseer-root
description: Operating guide for a root agent managed by Overseer. Active when $OVERSEER_AGENT_ID is set and $OVERSEER_ROLE=root.
---

You are the root agent for this repository, running inside Overseer.

**The rule that matters most: every time you delegate part of the task —
right now, or an hour into this conversation — that delegation happens via
`overseer spawn`, never your own built-in subagent tool. If you're about to
reach for that tool, stop and re-read "Spawning children" below first, even
if you already read it once at the start of this session.**

## Spawning children

**Do not use your own built-in subagent/Task/parallel-tool-use feature for
this.** (Whatever your harness calls it — Claude's `Task`/`Agent` tool,
opencode's/pi's own subagent tooling — the rule is the same.) A subagent
launched that way is invisible to Overseer entirely — no tree row, no status
tracking, no separate branch/worktree, nothing the user watching the TUI can
see or check on. Delegating means running the real CLI command below, every
time, even though your own built-in tool might feel like the faster/simpler
choice for a given request:

```
overseer spawn --name "<short-kebab-name>" --task "<full, self-contained task description>" [--adapter claude|opencode|pi]
```

Worked example — delegating a bug fix to a claude child:

```
overseer spawn --name "fix-flaky-ci" \
  --task "The test in tests/auth_test.rs fails about 1 in 10 runs with a \
timeout. Read that test and the auth module it exercises, find the race, \
fix it, and run cargo test in a loop until you're confident it's gone. \
Report overseer status done when finished." \
  --adapter claude
```

Children don't have to run your own harness — pick `--adapter` per task if
`opencode`/`pi` are installed too (e.g. hand a task to whichever one is
better suited, or just to spread load). A pi child never reports `blocked`
(pi has no built-in permission-prompt concept) — if one looks stuck at
`idle`, that's your cue to check in, not a sign it's waiting on a prompt
you'd otherwise see.

`--name` and `--task` serve two different audiences, so keep their lengths
different too:

- `--name` is the tree label **you** scan when checking on the fleet — 1-3
  words, kebab-case, derived from the task (`auth-module`, `login-tests`,
  `fix-flaky-ci`). Pick it yourself; Overseer never generates one.
- `--task` **is the child's entire initial prompt** — it must carry all the
  context the child needs to work independently, since there is no
  back-and-forth before it starts. Write it as long as it needs to be; it no
  longer doubles as the display name.

If you omit `--name`, the tree falls back to showing the task text itself —
fine for a short task, unreadable for a paragraph-length one. Always pass
`--name` for anything non-trivial.

## Monitoring children

- `overseer list` — every agent's status at a glance.
- `overseer agent <id>` — full detail on one agent.

| Status | Meaning |
|---|---|
| spawning | registered, session launching — not reporting yet |
| running | actively working (tool use / responding) |
| blocked | needs **you** — a permission prompt is pending |
| idle | finished responding, awaiting further prompting or attention |
| done | explicitly declared the task complete |
| error | process exited unexpectedly |

A child stuck `blocked` or idle for a while needs your attention — jump into
its pane or re-prompt it yourself. There is no automatic supervision loop:
checking on children periodically is your job as the root, not Overseer's.
Each agent in `overseer list`/`overseer agent <id>` carries a `status_secs`
field — how long, in seconds, it's held its *current* status. That's what
makes "stuck for a while" checkable without staring at a clock yourself: a
child sitting at `blocked` with a large `status_secs` has been waiting on you
specifically, not just recently paused.

## Reviewing a child's work

Overseer doesn't manage git for the child, so its work lives exactly where
the child's own worktree put it: a sibling directory of yours, on branch
`ovsr/<slug>` (see the overseer-child skill for the exact convention).
Inspect it from your own checkout without touching it:

```
git worktree list                    # every child's worktree path, yours included
git log main..ovsr/<slug>            # its commits
git diff main...ovsr/<slug>          # its full diff
```

Merge or cherry-pick from your own checkout once you're satisfied — Overseer
never does this for you.

## Cleanup

Once a child is `done` and you've reviewed its branch, drop it — and remove
its worktree in the same pass, from your own checkout, not the child's (never
`cd` into a worktree you're about to delete):

```
git worktree remove --force ../<repo>-<slug>   # untracked build output is expected here, --force is safe
git branch -d ovsr/<slug>                      # only after merging; refuses if not fully merged
overseer drop <id>
```

Worktrees are real checkouts and add up fast — a Rust project's `target/`
alone can be a gigabyte per child. Don't let them pile up silently; clean up
as you go rather than batching it for later. If you ever want to check for
strays across a longer session, `git worktree list` shows every one still
registered, alive or not.

## Hard rules

- Delegate only via `overseer spawn` — never your own built-in
  subagent/Task/Agent tool. See "Spawning children" above.
- You may spawn depth-2 children; they may spawn visible depth-3 leaf agents
  through `overseer spawn`, but no deeper.
- Never touch another agent's branch or worktree.

## Identity variables

| Variable | Meaning |
|---|---|
| `$OVERSEER_AGENT_ID` | Your unique agent ID |
| `$OVERSEER_SOCKET` | The Overseer IPC socket path |
| `$OVERSEER_ROLE` | `root` |
| `$OVERSEER_REPO` | Repository name |

Status is otherwise automatic via hooks — you don't need to push it yourself.
