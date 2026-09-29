---
name: overseer-integration
description: Implement or revise an Overseer harness adapter, its role bootstrap, lifecycle reporting, or installation migration. Use when integrating an agent CLI or changing how Overseer supplies session instructions.
---

# Overseer harness integrations

Keep delegation observable and interactive: a parent assigns work, a child reports
an outcome, and the parent reviews it. The runtime owns identity, admission limits,
and recorded state; instructions explain how to use those facilities.

## Establish the actual contract

- Read `AGENTS.md`, the crate architecture files, and the closest implementation
  in `crates/overseer-core/src/agent/adapters/`. Check configuration defaults,
  `install.rs`, and `settings.rs` before adding a new integration mechanism.
- Record the installed harness version. Inspect its `--help`, available source or
  schemas, and supported context/event interfaces. Use current official docs when
  those do not settle a question. A documented event is a hypothesis until observed.
- Verify the launch form accepts an assignment and stays interactive in its PTY.
  Preserve configured commands and extra arguments. Empty assignments must still
  open a session that a human can prompt.
- Declare capabilities according to evidence. A launch-only adapter can be useful;
  mark unavailable telemetry unsupported and unverified event mappings experimental.
  Do not infer permissions, completion, or context usage from screen text or silence.

## Supply one relevant role bootstrap

- Generate mandatory context from shared role semantics and current session facts:
  identity, role, depth, assignment when present, working directory, and available
  delegation/result actions. A child that can delegate is also a team lead for its
  own children. Respect the runtime depth and direct-child limits.
- Deliver only the applicable role through a verified harness context mechanism.
  Optional discoverable skills hold examples and troubleshooting; essential setup
  must not depend on a model discovering a skill or ignoring a second role document.
- If implementing a runtime context command, define and test that contract before
  referencing it in generated instructions. Do not document proposed commands as
  available. Keep harness-specific transport separate from shared role content.
- Cover startup, resume, and compaction: refresh current assignment and role through
  supported boundaries, without repeatedly appending identical instructions each turn.
- Recommend isolation when work needs it. Read-only and non-git tasks do not require
  worktrees. Preserve uncertain edits and stop sessions before removing directories;
  never assume a base branch or that forced worktree deletion is safe.

## Report facts without taking control

- Hooks/plugins must do nothing outside Overseer. Validate required identity/socket
  context, use the installed absolute Overseer executable path, and pass arguments
  without shell interpolation of task text or event data.
- Filter events to the harness session associated with this Overseer agent. Native
  nested sessions must not overwrite their parent's status. Treat session identity
  and Overseer node identity as separate values, including across resume.
- Separate lifecycle activity from explicit task completion and review. A turn ending
  means idle; later tool events must not erase a recorded outcome. Preserve attention
  until its relevant condition resolves. Respect ordering information at the boundary.
- Preserve the harness's approvals and trust settings. Integration code observes
  permission requests; it does not answer them or weaken policy to make probes pass.

## Install and migrate narrowly

Use the existing merge strategies where possible. Preserve unrelated hooks,
instructions, configuration values, and symlink targets. Invalid configuration must
remain intact with a useful error. Test repeated installation, upgrade from the old
role-document layout, and uninstall; remove only identifiable Overseer-owned content.
Prefer supported per-session context delivery when it avoids global configuration.

## Verify a small end-to-end path

1. Add a failing regression for the changed behavior when applicable. Exercise
   generated scripts with sanitized event fixtures and a recording fake executable;
   string-presence assertions alone do not verify argument handling or event filtering.
2. Cover outside-Overseer no-op, root/child context, unrelated session events, empty
   assignment, resume/compaction delivery, install migration, and capability gaps as
   relevant to the change. Test user configuration preservation semantically.
3. Use an isolated socket and temporary config for executable probes. Start with
   help/schema checks, launch checks, and synthetic events. Make live provider calls
   only when necessary and authorized; otherwise report the remaining verification
   gap and keep the corresponding capability experimental. Do not change the user's
   installed harness configuration merely to run a test.
4. Run focused tests, then the required workspace checks. For changed render/event
   paths, check responsiveness with a few local sessions and bounded event bursts.
   Fleet stress tests are optional and should follow a demonstrated performance need.
5. Keep commits focused on one reviewable behavior. Update supported-version evidence,
   capability descriptions, and `AGENTS.md` to describe what actually ships. Report
   which executable behavior was verified and which remains unverified.
