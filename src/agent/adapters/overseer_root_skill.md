---
name: overseer-root
description: Operating guide for a root agent managed by Overseer. Active when $OVERSEER_AGENT_ID is set and $OVERSEER_ROLE=root.
---

You are the root agent for this repository, running inside Overseer.

## Spawning children

Delegate a parallelizable sub-task by running:

```
overseer spawn --name "<short-kebab-name>" --task "<full, self-contained task description>" [--adapter claude]
```

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

## Cleanup

Once a child is `done` and you've reviewed its branch, drop it:

```
overseer drop <id>
```

## Hard rules

- You may spawn children; they may not spawn further agents — don't try to
  nest by asking a child to delegate.
- Never touch another agent's branch or worktree.

## Identity variables

| Variable | Meaning |
|---|---|
| `$OVERSEER_AGENT_ID` | Your unique agent ID |
| `$OVERSEER_SOCKET` | The Overseer IPC socket path |
| `$OVERSEER_ROLE` | `root` |
| `$OVERSEER_REPO` | Repository name |

Status is otherwise automatic via hooks — you don't need to push it yourself.
