use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use super::{AdapterCapabilities, AgentAdapter, CapabilitySupport, InstalledFile, LaunchContext, MergeStrategy};

const PLUGIN_PATH: &str = "plugin/overseer.js";
const ROOT_INSTRUCTIONS_PATH: &str = "overseer-root.md";
const CHILD_INSTRUCTIONS_PATH: &str = "overseer-child.md";
const CONFIG_PATH: &str = "opencode.jsonc";
const INSTRUCTIONS_KEY: &str = "instructions";

const ROOT_INSTRUCTIONS_CONTENT: &str = include_str!("opencode_root.md");
const CHILD_INSTRUCTIONS_CONTENT: &str = include_str!("opencode_child.md");

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
    fn plugin_content(&self) -> String {
        let bin = serde_json::to_string(&self.overseer_bin.to_string_lossy().to_string())
            .unwrap_or_else(|_| "\"overseer\"".to_string());
        format!(
            r#"const OVERSEER_BIN = {bin};

export const OverseerPlugin = async () => {{
  if (!process.env.OVERSEER_AGENT_ID) {{
    return {{}};
  }}
  const {{ execFile }} = await import("node:child_process");
  const push = (status, extra = []) => execFile(OVERSEER_BIN, ["status", status, ...extra], (error) => {{
    if (error && process.env.OVERSEER_DEBUG) console.error(`overseer status push failed: ${{error.code || "unknown"}}`);
  }});

  return {{
    event: async ({{ event }}) => {{
      if (event.type === "session.created") {{
        // Roots and taskless TUI-created children wait for a human prompt;
        // CLI-spawned children already have their initial task.
        const initial = process.env.OVERSEER_TASK ? "running" : "idle";
        execFile(OVERSEER_BIN, ["status", initial, "--adapter", "opencode", "--clear-context"], () => {{}});
      }} else if (event.type === "session.status" && event.properties?.status?.type === "busy") {{
        push("running");
      }} else if (event.type === "session.idle") {{
        push("idle");
      }} else if (event.type === "permission.asked" || event.type === "permission.v2.asked") {{
        push("blocked", ["--attention", "permission"]);
      }} else if (event.type === "permission.replied" || event.type === "permission.v2.replied") {{
        push("running", ["--clear-attention", "permission"]);
      }} else if (event.type === "session.error" && event.properties?.error?.name === "APIError") {{
        const error = event.properties.error.data || {{}};
        const status = error.statusCode;
        const kind = status === 429 ? "rate-limit" : status === 402 ? "billing" : "provider-error";
        const extra = ["--attention", kind];
        if (typeof error.message === "string") extra.push("--message", error.message.slice(0, 4096));
        const retry = error.responseHeaders?.["retry-after"] ?? error.responseHeaders?.["Retry-After"];
        if (retry) extra.push("--retry-after", String(retry));
        push("running", extra);
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
                path: PathBuf::from(ROOT_INSTRUCTIONS_PATH),
                content: ROOT_INSTRUCTIONS_CONTENT.to_string(),
                merge: MergeStrategy::Overwrite,
            },
            InstalledFile {
                path: PathBuf::from(CHILD_INSTRUCTIONS_PATH),
                content: CHILD_INSTRUCTIONS_CONTENT.to_string(),
                merge: MergeStrategy::Overwrite,
            },
            // Both role docs are always registered — each one's own opening
            // line ("only applies when $OVERSEER_ROLE=...") is what makes
            // loading both, every session, harmless (same self-filtering
            // posture as Claude's root/child skills, which both install
            // unconditionally too).
            InstalledFile {
                path: PathBuf::from(CONFIG_PATH),
                content: String::new(),
                merge: MergeStrategy::JsonArrayMerge {
                    key: INSTRUCTIONS_KEY,
                    entries: vec![
                        ROOT_INSTRUCTIONS_PATH.to_string(),
                        CHILD_INSTRUCTIONS_PATH.to_string(),
                    ],
                },
            },
        ]
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
    fn install_files_returns_plugin_two_instructions_and_config_merge() {
        let a = make_adapter();
        let files = a.install_files();
        assert_eq!(files.len(), 4);
        assert_eq!(files[0].path, PathBuf::from(PLUGIN_PATH));
        assert!(matches!(files[0].merge, MergeStrategy::Overwrite));
        assert_eq!(files[1].path, PathBuf::from(ROOT_INSTRUCTIONS_PATH));
        assert_eq!(files[2].path, PathBuf::from(CHILD_INSTRUCTIONS_PATH));
        assert_eq!(files[3].path, PathBuf::from(CONFIG_PATH));
        match &files[3].merge {
            MergeStrategy::JsonArrayMerge { key, entries } => {
                assert_eq!(*key, INSTRUCTIONS_KEY);
                assert_eq!(entries, &vec![ROOT_INSTRUCTIONS_PATH.to_string(), CHILD_INSTRUCTIONS_PATH.to_string()]);
            }
            _ => panic!("expected JsonArrayMerge for the config file"),
        }
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
    fn root_instructions_guard_on_role() {
        assert!(ROOT_INSTRUCTIONS_CONTENT.contains("OVERSEER_ROLE=root"));
    }

    #[test]
    fn root_instructions_forbid_the_built_in_subagent_tool_for_delegation() {
        // A real user reported the model using its own Task/subagent tool
        // instead of `overseer spawn` — those subagents are invisible to
        // Overseer entirely (no tree row, no tracking).
        assert!(ROOT_INSTRUCTIONS_CONTENT.to_lowercase().contains("do not use your own built-in subagent"));
    }

    #[test]
    fn child_instructions_guard_on_role_and_document_done_status() {
        assert!(CHILD_INSTRUCTIONS_CONTENT.contains("OVERSEER_ROLE=child"));
        assert!(CHILD_INSTRUCTIONS_CONTENT.contains("overseer status done"));
    }

    #[test]
    fn child_instructions_require_visible_delegation_and_document_depth_three() {
        assert!(CHILD_INSTRUCTIONS_CONTENT.contains("never your harness's built-in"));
        assert!(CHILD_INSTRUCTIONS_CONTENT.contains("read-only lookup"));
        assert!(CHILD_INSTRUCTIONS_CONTENT.contains("OVERSEER_DEPTH"));
    }

    #[test]
    fn root_instructions_bless_cross_harness_spawn() {
        assert!(ROOT_INSTRUCTIONS_CONTENT.contains("--adapter claude|opencode"));
    }

    #[test]
    fn child_instructions_document_the_worktree_convention_with_a_worked_example() {
        // A one-sentence "set up your own git worktree/branch" with no
        // example was the reported gap: an agent given nothing more had to
        // be manually corrected into a naming convention by hand each time.
        assert!(CHILD_INSTRUCTIONS_CONTENT.contains("git worktree add"), "must show a runnable worktree command");
        assert!(CHILD_INSTRUCTIONS_CONTENT.contains("ovsr/<slug>"), "must state the branch naming convention");
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
