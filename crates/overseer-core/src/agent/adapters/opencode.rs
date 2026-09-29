use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use super::{AdapterCapabilities, AgentAdapter, CapabilitySupport, InstalledFile, LaunchContext, MergeStrategy};

const PLUGIN_PATH: &str = "plugin/overseer.js";
const ROOT_INSTRUCTIONS_PATH: &str = "overseer-root.md";
const CHILD_INSTRUCTIONS_PATH: &str = "overseer-child.md";
const CONFIG_PATH: &str = "opencode.jsonc";
const INSTRUCTIONS_KEY: &str = "instructions";

pub struct OpencodeAdapter {
    overseer_bin: PathBuf,
}

impl OpencodeAdapter {
    pub fn new() -> Self {
        let overseer_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("overseer"));
        Self { overseer_bin }
    }

    #[cfg(test)]
    pub fn with_bin(overseer_bin: PathBuf) -> Self {
        Self { overseer_bin }
    }

    /// The plugin auto-loads from `plugin/*.js` under opencode's config dir —
    /// no entry in `opencode.jsonc`'s `plugin` array needed (verified live,
    /// HARNESSES.md Task 0: a file dropped there loads and fires on the very
    /// next session with no registration step). It no-ops instantly unless
    /// `$OVERSEER_AGENT_ID` is set (same conditional posture as every other
    /// Overseer-managed hook).
    ///
    /// Event mapping re-verified against opencode 1.17.20 types and live root
    /// and child sessions. The top-level typed `permission.ask` hook did not
    /// fire; the generic event bus emitted `permission.asked` and
    /// `permission.replied`, so those are the supported conformance path.
    /// - `session.status`'s `properties.status.type === "busy"` is the actual
    ///   "the agent is actively working" signal — better than proxying via
    ///   `tool.execute.after`, which only fires around tool calls, not while
    ///   the model is just thinking/responding.
    ///
    /// No `--from-hook`: that flag drives Claude-transcript-specific
    /// classification (`agent::hook`) that doesn't apply here — the plugin's
    /// events are already precise, so pushes go straight to `overseer status`.
    ///
    /// No explicit `--branch` either: `execFile` without an `options.cwd`
    /// inherits `process.cwd()` from the opencode process itself, so the
    /// `overseer` binary this spawns sees the same cwd opencode does —
    /// `cli::detect_current_branch`'s `git rev-parse --abbrev-ref HEAD`
    /// (run unconditionally, not gated on `--from-hook`) self-reports the
    /// real branch for every push with zero JS-side git shelling needed.
    fn plugin_content(&self) -> String {
        let bin = serde_json::to_string(&self.overseer_bin.to_string_lossy().to_string())
            .unwrap_or_else(|_| "\"overseer\"".to_string());
        format!(
            r#"const OVERSEER_BIN = {bin};

export const OverseerPlugin = async ({{ client }}) => {{
  if (!process.env.OVERSEER_AGENT_ID || !process.env.OVERSEER_SOCKET) {{
    return {{}};
  }}
  const {{ execFile }} = await import("node:child_process");
  // The system/compaction hooks are experimental interfaces, verified against
  // installed @opencode-ai/plugin 1.17.13 with OpenCode 1.17.20. Native nested
  // sessions must not receive this node's role or overwrite its telemetry.
  let mainSession;
  let bootstrap;
  const isMainSession = async (id, activate = false) => {{
    if (!id) return false;
    if (id === mainSession) return true;
    if (mainSession && !activate) return false;
    try {{
      const result = await client.session.get({{ path: {{ id }}, signal: AbortSignal.timeout(2000) }});
      if (!result.data || result.data.parentID) return false;
      mainSession = id;
      bootstrap = undefined;
      return true;
    }} catch {{ return false; }}
  }};
  const context = async (refresh = false) => {{
    if (refresh) bootstrap = undefined;
    if (!bootstrap) {{
      bootstrap = new Promise((resolve) => execFile(OVERSEER_BIN, ["context"],
        {{ timeout: 2000, maxBuffer: 64 * 1024 }}, (error, stdout) => {{
          if (error) bootstrap = undefined;
          resolve(error ? "" : stdout.trim());
        }}));
    }}
    return bootstrap;
  }};
  const push = (status, extra = []) => new Promise((resolve) =>
    execFile(OVERSEER_BIN, ["status", status, ...extra], {{ timeout: 2000 }}, (error) => {{
      if (error && process.env.OVERSEER_DEBUG) console.error(`overseer status push failed: ${{error.code || "unknown"}}`);
      resolve();
    }}));

  return {{
    "experimental.chat.system.transform": async (input, output) => {{
      if (!await isMainSession(input.sessionID, true)) return;
      const text = await context();
      if (text && !output.system.some((entry) => entry === text)) output.system.push(text);
    }},
    "experimental.session.compacting": async (input, output) => {{
      if (!await isMainSession(input.sessionID)) return;
      const text = await context(true);
      if (text) output.context.push(text);
    }},
    "chat.message": async (input) => {{
      if (!await isMainSession(input.sessionID, true)) return;
      if (input.model?.modelID) {{
        const model = input.model.providerID ? `${{input.model.providerID}}/${{input.model.modelID}}` : input.model.modelID;
        await push("running", ["--model-name", model]);
      }}
    }},
    event: async ({{ event }}) => {{
      const info = event.properties?.info;
      const id = event.properties?.sessionID || info?.id;
      if (info?.parentID || !await isMainSession(id)) return;
      if (event.type === "session.created") {{
        // Roots and taskless TUI-created children wait for a human prompt;
        // CLI-spawned children already have their initial task.
        const initial = process.env.OVERSEER_TASK ? "running" : "idle";
        await push(initial, ["--adapter", "opencode", "--clear-context"]);
      }} else if (event.type === "session.status" && event.properties?.status?.type === "busy") {{
        await push("running");
      }} else if (event.type === "session.idle") {{
        await push("idle");
      }} else if (event.type === "permission.asked" || event.type === "permission.v2.asked") {{
        await push("blocked", ["--attention", "permission"]);
      }} else if (event.type === "permission.replied" || event.type === "permission.v2.replied") {{
        await push("running", ["--clear-attention", "permission"]);
      }} else if (event.type === "session.error" && event.properties?.error?.name === "APIError") {{
        const error = event.properties.error.data || {{}};
        const status = error.statusCode;
        const kind = status === 429 ? "rate-limit" : status === 402 ? "billing" : "provider-error";
        const extra = ["--attention", kind];
        if (typeof error.message === "string") extra.push("--message", error.message.slice(0, 4096));
        const retry = error.responseHeaders?.["retry-after"] ?? error.responseHeaders?.["Retry-After"];
        if (retry) extra.push("--retry-after", String(retry));
        await push("running", extra);
      }}
    }},
  }};
}};
"#
        )
    }
}

impl Default for OpencodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentAdapter for OpencodeAdapter {
    fn capabilities(&self) -> AdapterCapabilities {
        // Live-probed with opencode 1.17.20. Permission is emitted on the
        // generic event bus as permission.asked/replied; the documented typed
        // permission.ask hook was not invoked. APIError is structured but a
        // real provider-limit response was not available to trigger safely.
        AdapterCapabilities {
            lifecycle: CapabilitySupport::Supported,
            permission_requests: CapabilitySupport::Supported,
            provider_limits: CapabilitySupport::Experimental {
                note: "structured APIError status is available; real limit response not yet live-probed".to_string(),
            },
            context_usage: CapabilitySupport::Unsupported {
                reason: "events expose token counts but not the active context-window size".to_string(),
            },
        }
    }

    fn user_config_dir(&self) -> Option<PathBuf> {
        if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
            return Some(PathBuf::from(dir).join("opencode"));
        }
        dirs::home_dir().map(|h| h.join(".config").join("opencode"))
    }

    fn is_installed(&self) -> bool {
        // `spawn_command` doesn't reference this path directly (opencode
        // auto-discovers plugin/*.js on its own), so a missing file doesn't
        // crash the launch -- but it does mean the session
        // never reports a single status, sitting at `spawning` forever,
        // which reads exactly as "did this even work?" too.
        self.user_config_dir().is_some_and(|dir| dir.join(PLUGIN_PATH).exists())
    }

    fn install_files(&self) -> Vec<InstalledFile> {
        vec![
            InstalledFile {
                path: PathBuf::from(PLUGIN_PATH),
                content: self.plugin_content(),
                merge: MergeStrategy::Overwrite,
            },
            InstalledFile {
                path: PathBuf::from(CONFIG_PATH),
                content: String::new(),
                merge: MergeStrategy::JsonArrayRemove {
                    key: INSTRUCTIONS_KEY,
                    entries: vec![
                        ROOT_INSTRUCTIONS_PATH.to_string(),
                        CHILD_INSTRUCTIONS_PATH.to_string(),
                    ],
                },
            },
        ]
    }

    fn legacy_paths(&self) -> Vec<PathBuf> {
        vec![ROOT_INSTRUCTIONS_PATH.into(), CHILD_INSTRUCTIONS_PATH.into()]
    }

    fn spawn_command(&self, ctx: &LaunchContext) -> Command {
        let mut cmd = Command::new(&ctx.command);
        for arg in &ctx.extra_args {
            cmd.arg(arg);
        }
        // Verified live (HARNESSES.md Task 0): `--prompt` seeds and
        // auto-submits the initial message while staying in the normal
        // interactive TUI -- unlike `opencode run`, which is one-shot.
        if !ctx.task.is_empty() {
            cmd.arg("--prompt").arg(&ctx.task);
        }
        cmd
    }

    fn env_inject(&self, ctx: &LaunchContext) -> HashMap<String, String> {
        let mut env = super::identity_env(&ctx.identity());
        if !ctx.task.is_empty() {
            env.insert("OVERSEER_TASK".to_string(), ctx.task.clone());
        }
        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentId, AgentRole};

    fn make_adapter() -> OpencodeAdapter {
        OpencodeAdapter::with_bin(PathBuf::from("/usr/local/bin/overseer"))
    }

    fn make_root_ctx() -> LaunchContext {
        LaunchContext {
            agent_id: AgentId::new(),
            role: AgentRole::Root,
            parent_id: None,
            socket: PathBuf::from("/tmp/overseer.sock"),
            cwd: PathBuf::from("/projects/myrepo"),
            repo: "myrepo".to_string(),
            command: "opencode".to_string(),
            extra_args: vec![],
            task: String::new(),
            depth: 1,
        }
    }

    fn make_child_ctx(parent: AgentId) -> LaunchContext {
        LaunchContext {
            agent_id: AgentId::new(),
            role: AgentRole::Child,
            parent_id: Some(parent),
            socket: PathBuf::from("/tmp/overseer.sock"),
            cwd: PathBuf::from("/projects/myrepo"),
            repo: "myrepo".to_string(),
            command: "opencode".to_string(),
            extra_args: vec![],
            task: "write unit tests for the login flow".to_string(),
            depth: 2,
        }
    }

    #[test]
    fn install_files_registers_plugin_and_removes_old_global_role_instructions() {
        let files = make_adapter().install_files();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, PathBuf::from(PLUGIN_PATH));
        assert!(matches!(files[0].merge, MergeStrategy::Overwrite));
        assert_eq!(files[1].path, PathBuf::from(CONFIG_PATH));
        match &files[1].merge {
            MergeStrategy::JsonArrayRemove { key, entries } => {
                assert_eq!(*key, INSTRUCTIONS_KEY);
                assert_eq!(entries, &vec![ROOT_INSTRUCTIONS_PATH.to_string(), CHILD_INSTRUCTIONS_PATH.to_string()]);
            }
            _ => panic!("expected removal of legacy instructions"),
        }
        assert_eq!(make_adapter().legacy_paths(), vec![PathBuf::from(ROOT_INSTRUCTIONS_PATH), PathBuf::from(CHILD_INSTRUCTIONS_PATH)]);
    }

    #[test]
    fn capabilities_match_live_probed_opencode_1_17_20() {
        let capabilities = make_adapter().capabilities();
        assert_eq!(capabilities.lifecycle, CapabilitySupport::Supported);
        assert_eq!(capabilities.permission_requests, CapabilitySupport::Supported);
        assert!(matches!(capabilities.provider_limits, CapabilitySupport::Experimental { .. }));
        assert!(matches!(capabilities.context_usage, CapabilitySupport::Unsupported { .. }));
    }

    #[test]
    fn plugin_guards_on_agent_id_and_embeds_absolute_bin_path() {
        let a = make_adapter();
        let content = a.plugin_content();
        assert!(content.contains("process.env.OVERSEER_AGENT_ID"));
        assert!(content.contains("/usr/local/bin/overseer"));
    }

    #[test]
    fn plugin_maps_session_created_status_from_task_presence() {
        let content = make_adapter().plugin_content();
        assert!(content.contains(r#""session.created""#));
        assert!(content.contains("process.env.OVERSEER_TASK"));
        assert!(content.contains(r#""idle""#));
        assert!(content.contains(r#""busy""#));
    }

    #[test]
    fn plugin_self_identifies_as_opencode_only_on_session_created() {
        // The only place this needs saying — a bare-shell root's registered
        // adapter is always "shell" until the real harness inside it says
        // otherwise; this is what an omitted --adapter on a later
        // `overseer spawn` inherits.
        let content = make_adapter().plugin_content();
        assert!(content.contains(r#""--adapter", "opencode""#));
        // Every other push (busy/idle/permission.replied/permission.ask)
        // stays a plain two-element argv — only session.created's gets the
        // extra pair.
        let adapter_occurrences = content.matches("--adapter").count();
        assert_eq!(adapter_occurrences, 1, "adapter self-id should appear exactly once: {content}");
    }

    #[test]
    fn plugin_maps_idle_and_permission_events() {
        let content = make_adapter().plugin_content();
        assert!(content.contains(r#""session.idle""#));
        assert!(content.contains(r#""permission.asked""#));
        assert!(content.contains(r#""permission.v2.asked""#));
        assert!(content.contains(r#""permission.replied""#));
        assert!(content.contains(r#""--attention", "permission""#));
        assert!(content.contains(r#""--clear-attention", "permission""#));
    }

    #[test]
    fn plugin_reports_the_typed_chat_message_model() {
        let content = make_adapter().plugin_content();
        assert!(content.contains(r#""chat.message""#));
        assert!(content.contains("input.model.providerID"));
        assert!(content.contains("input.model.modelID"));
        assert!(content.contains(r#""--model-name""#));
    }

    #[test]
    fn plugin_never_passes_an_explicit_branch() {
        // Every push relies on the shared `overseer` CLI's own cwd-based
        // auto-detection (`cli::detect_current_branch`) instead — `execFile`
        // with no `options.cwd` inherits opencode's own process cwd, so the
        // plugin needs no JS-side git shelling.
        let content = make_adapter().plugin_content();
        assert!(!content.contains("--branch"));
    }

    #[test]
    fn plugin_never_writes_a_permission_decision() {
        // Overseer only observes the permission prompt; it must never
        // auto-resolve it (that decision stays the human's).
        let content = make_adapter().plugin_content();
        assert!(!content.contains("output.status"));
        assert!(!content.contains(r#""permission.ask""#));
    }

    #[test]
    fn captured_permission_fixture_matches_the_generic_event_bus_shape() {
        let ask: serde_json::Value = serde_json::from_str(
            r#"{"type":"permission.asked","properties":{"id":"redacted","sessionID":"redacted","permission":"bash","patterns":[],"metadata":{},"always":[],"tool":{"messageID":"redacted","callID":"redacted"}}}"#,
        )
        .unwrap();
        let reply: serde_json::Value = serde_json::from_str(
            r#"{"type":"permission.replied","properties":{"sessionID":"redacted","requestID":"redacted","reply":"reject"}}"#,
        )
        .unwrap();
        assert_eq!(ask["type"], "permission.asked");
        assert_eq!(reply["type"], "permission.replied");
        assert!(make_adapter().plugin_content().contains("event.type === \"permission.asked\""));
    }

    #[test]
    fn structured_api_error_fixture_maps_only_status_codes_not_display_text() {
        let fixture: serde_json::Value = serde_json::from_str(
            r#"{"type":"session.error","properties":{"sessionID":"redacted","error":{"name":"APIError","data":{"message":"redacted","statusCode":429,"isRetryable":true,"responseHeaders":{"retry-after":"30"}}}}}"#,
        )
        .unwrap();
        assert_eq!(fixture["properties"]["error"]["name"], "APIError");
        assert_eq!(fixture["properties"]["error"]["data"]["statusCode"], 429);
        let content = make_adapter().plugin_content();
        assert!(content.contains("status === 429"));
        assert!(!content.contains("includes("), "provider classification must not match display text");
    }

    #[test]
    fn plugin_injects_cached_role_context_and_filters_nested_sessions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("overseer-plugin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("overseer's executable");
        std::fs::write(&bin, r#"#!/bin/sh
printf '%s\n' "$*" >> "$OVERSEER_TEST_LOG"
if [ "$1" = context ]; then printf 'role:%s\n' "$OVERSEER_ROLE"; fi
"#).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        let plugin = dir.join("plugin.mjs");
        std::fs::write(&plugin, OpencodeAdapter::with_bin(bin).plugin_content()).unwrap();
        let fixture = dir.join("fixture.mjs");
        std::fs::write(&fixture, r#"
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
const { OverseerPlugin } = await import(pathToFileURL(process.argv[2]));
const client = { session: { get: async ({path}) => ({data: {id: path.id, parentID: path.id === 'nested' ? 'main' : undefined}}) } };
delete process.env.OVERSEER_AGENT_ID;
assert.deepEqual(await OverseerPlugin({client}), {});
process.env.OVERSEER_AGENT_ID = 'agent';
delete process.env.OVERSEER_SOCKET;
assert.deepEqual(await OverseerPlugin({client}), {});
process.env.OVERSEER_SOCKET = '/tmp/test.sock';
for (const role of ['root', 'child']) {
  process.env.OVERSEER_ROLE = role;
  const hooks = await OverseerPlugin({client});
  const input = { sessionID: 'main', model: {providerID: 'provider', modelID: 'model'} };
  const transform = hooks['experimental.chat.system.transform'];
  const system = { system: ['user context'] };
  await transform(input, system);
  await transform(input, system);
  assert.deepEqual(system.system, ['user context', `role:${role}`]);
  const nested = {system: []};
  await transform({...input, sessionID: 'nested'}, nested);
  assert.deepEqual(nested.system, []);
  await hooks['chat.message']({...input, sessionID: 'nested'});
  await hooks.event({event: {type: 'permission.asked', properties: {sessionID: 'nested'}}});
  await hooks.event({event: {type: 'session.created', properties: {info: {id: 'nested', parentID: 'main'}}}});
  const compact = {context: ['existing']};
  await hooks['experimental.session.compacting'](input, compact);
  assert.deepEqual(compact.context, ['existing', `role:${role}`]);
  await hooks['chat.message'](input);
  await hooks.event({event: {type: 'permission.asked', properties: {sessionID: 'main'}}});
  await hooks.event({event: {type: 'session.error', properties: {sessionID: 'main', error: {name: 'APIError', data: {statusCode: 429}}}}});
}
const lines = readFileSync(process.env.OVERSEER_TEST_LOG, 'utf8').trim().split('\n');
assert.equal(lines.filter(line => line === 'context').length, 4, 'context cached per session and refreshed at compaction');
assert.equal(lines.filter(line => line.startsWith('status ')).length, 6, 'only main-session events report status');
assert.equal(lines.filter(line => line.includes('--attention permission')).length, 2);
assert.equal(lines.filter(line => line.includes('--attention rate-limit')).length, 2);
"#).unwrap();
        let output = Command::new("node").arg(&fixture).arg(&plugin)
            .env("OVERSEER_TEST_LOG", dir.join("calls.log")).output()
            .expect("node is required to validate generated OpenCode plugins");
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        std::fs::remove_dir_all(dir).unwrap();
        assert!(output.status.success(), "generated plugin failed its executable fixture: {stderr}");
    }

    #[test]
    fn spawn_command_with_task_uses_prompt_flag_not_positional() {
        let a = make_adapter();
        let ctx = make_child_ctx(AgentId::new());
        let cmd = a.spawn_command(&ctx);
        assert_eq!(cmd.get_program(), "opencode");
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert_eq!(args, vec!["--prompt".to_string(), "write unit tests for the login flow".to_string()]);
    }

    #[test]
    fn spawn_command_empty_task_appends_nothing() {
        let a = make_adapter();
        let ctx = make_root_ctx();
        let cmd = a.spawn_command(&ctx);
        assert_eq!(cmd.get_args().count(), 0);
    }

    #[test]
    fn env_inject_child_includes_overseer_task() {
        let a = make_adapter();
        let ctx = make_child_ctx(AgentId::new());
        let env = a.env_inject(&ctx);
        assert_eq!(env.get("OVERSEER_TASK").map(String::as_str), Some("write unit tests for the login flow"));
    }

    #[test]
    fn env_inject_root_has_no_overseer_task() {
        let a = make_adapter();
        let ctx = make_root_ctx();
        let env = a.env_inject(&ctx);
        assert!(!env.contains_key("OVERSEER_TASK"));
    }

}
