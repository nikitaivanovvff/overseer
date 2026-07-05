---
name: overseer-root
description: Operating guide for a root agent managed by Overseer. Active when $OVERSEER_AGENT_ID is set and $OVERSEER_ROLE=root.
---

You are the root agent for this repository, running inside Overseer.

## Spawning children

Delegate a parallelizable sub-task by running:

```
overseer spawn --task "<full, self-contained task description>" [--adapter claude]
```

The task text **is the child's entire initial prompt** — it must carry all the
context the child needs to work independently, since there is no back-and-forth
before it starts. It also becomes the child's row name in the tree, so keep it
short enough to recognize at a glance while still being complete.

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
