---
name: overseer-child
description: Operating guide for a child agent managed by Overseer. Active when $OVERSEER_AGENT_ID is set and $OVERSEER_ROLE=child.
---

You are a child agent spawned by another agent, running inside Overseer.

## Your assignment

`$OVERSEER_TASK` holds the task you were spawned with. Your initial prompt
already contains it, so you don't need to read the variable to get started —
but re-read it if you ever lose track of what you were asked to do.

## Isolation

Set up your own git worktree/branch before making any changes. Overseer does
not manage workspaces — it only launched this session in the repo, nothing
more.

Convention: branch `ovsr/<slug>`, worktree as a sibling directory named
`<repo>-<slug>`, where `<slug>` is a short kebab-case name you pick from your
own task (mirroring how the root names you in the tree). Worked example:

```
git worktree add ../$OVERSEER_REPO-<slug> -b ovsr/<slug>
cd ../$OVERSEER_REPO-<slug>
```

Do all your work from inside that directory, not the one you were launched
in. If that branch or path already exists (a sibling child picked the same
slug), pick a more specific slug and retry — never reuse or touch another
agent's branch or worktree.

## Completion

When the task is genuinely complete, report it explicitly:

```
overseer status done --message "<one-line summary>"
```

This is the **only** way your status becomes `done` — Overseer never infers
completion from you going quiet or from the session ending.

## Hard rules

- Delegate real sub-tasks with `overseer spawn`, never your harness's built-in
  subagent or Task tool. Built-in subagents are invisible to the user watching
  Overseer's tree. A single-shot, read-only lookup that finishes well under a
  minute may stay in-harness; anything longer, file-writing, or substantial
  must be a visible Overseer child.
- Check `$OVERSEER_DEPTH` before delegating. At depth `3` you cannot spawn;
  perform the work inline. At depth `2`, spawned agents are depth-3 leaves.
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
| `$OVERSEER_DEPTH` | Tree depth (`2` or `3`); depth `3` is the spawn limit |

Status is otherwise automatic via hooks — you don't need to push it yourself,
except for the explicit `done` above.
