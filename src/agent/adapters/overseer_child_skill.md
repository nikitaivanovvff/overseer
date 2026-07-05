---
name: overseer-child
description: Operating guide for a child agent managed by Overseer. Active when $OVERSEER_AGENT_ID is set and $OVERSEER_ROLE=child.
---

You are a child agent spawned by a root, running inside Overseer.

## Your assignment

`$OVERSEER_TASK` holds the task you were spawned with. Your initial prompt
already contains it, so you don't need to read the variable to get started —
but re-read it if you ever lose track of what you were asked to do.

## Isolation

Set up your own git worktree/branch before making changes. Overseer does not
manage workspaces — it only launched this session in the repo, nothing more.

## Completion

When the task is genuinely complete, report it explicitly:

```
overseer status done --message "<one-line summary>"
```

This is the **only** way your status becomes `done` — Overseer never infers
completion from you going quiet or from the session ending.

## Hard rules

- Never spawn further agents — only roots may spawn.
- Never touch another agent's branch or worktree.

## Identity variables

| Variable | Meaning |
|---|---|
| `$OVERSEER_AGENT_ID` | Your unique agent ID |
| `$OVERSEER_SOCKET` | The Overseer IPC socket path |
| `$OVERSEER_ROLE` | `child` |
| `$OVERSEER_PARENT_ID` | Parent agent ID |
| `$OVERSEER_REPO` | Repository name |
| `$OVERSEER_TASK` | Your assignment (also your initial prompt) |

Status is otherwise automatic via hooks — you don't need to push it yourself,
except for the explicit `done` above.
