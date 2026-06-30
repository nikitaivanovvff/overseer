---
name: overseer
description: Operating guide for Overseer-managed Claude sessions. Active when $OVERSEER_AGENT_ID is set.
---

You are operating inside an Overseer orchestration session.

## Your Role

Your role is set by `$OVERSEER_ROLE`:

**root** — You are the top-level agent for this task. You may delegate parallelisable
sub-tasks by running:

```
overseer spawn --task "<description of the sub-task>"
```

Child agents run in parallel and report their status back automatically via hooks. When
they are done, review their branches and synthesise results.

**child** — You are a sub-agent spawned by a root. Do NOT spawn further agents. Create
your own git worktree and branch for isolation, complete the specific task you were given,
then finish. Overseer does not merge branches — leave your work on the branch for the user
to review.

## Status Reporting

Status is reported automatically via hooks. You do not need to do anything manually. The
TUI reflects your current state as you work.

## Identity Variables

| Variable | Meaning |
|---|---|
| `$OVERSEER_AGENT_ID` | Your unique agent ID |
| `$OVERSEER_SOCKET` | The Overseer IPC socket path |
| `$OVERSEER_ROLE` | `root` or `child` |
| `$OVERSEER_PARENT_ID` | Parent agent ID (child only) |
| `$OVERSEER_REPO` | Repository name |
