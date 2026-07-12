use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use super::{AgentAdapter, InstalledFile, LaunchContext, MergeStrategy};

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
    /// Event mapping verified against the installed `@opencode-ai/sdk` type
    /// definitions and a live session (HARNESSES.md Task 0), not the spec's
    /// original guess — two corrections worth calling out:
    /// - The "a permission is being asked" moment is `permission.ask`, a
    ///   *separate* hook (not part of the generic `event` bus) that opencode
    ///   calls before every prompt; the bus's own `permission.updated` never
    ///   fired for a same-process plugin in testing. `permission.replied`
    ///   *does* fire on the bus and is used to push back to `running`.
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
  const push = (status) => execFile(OVERSEER_BIN, ["status", status], () => {{}});

  return {{
    event: async ({{ event }}) => {{
      if (event.type === "session.created") {{
        // Root (bare shell the human ran opencode inside) is waiting on the
        // human to prompt it, not doing anything yet -- pushes idle. A
        // spawned child already has its task as the initial prompt and is
        // working the instant it launches -- pushes running.
        const initial = process.env.OVERSEER_ROLE === "root" ? "idle" : "running";
        execFile(OVERSEER_BIN, ["status", initial, "--adapter", "opencode"], () => {{}});
      }} else if (event.type === "session.status" && event.properties?.status?.type === "busy") {{
        push("running");
      }} else if (event.type === "session.idle") {{
        push("idle");
      }} else if (event.type === "permission.replied") {{
        push("running");
      }}
      // session.error: nothing -- the exit watcher owns error, not a lifecycle push.
    }},
    "permission.ask": async () => {{
      push("blocked");
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
    fn user_config_dir(&self) -> Option<PathBuf> {
        if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
            return Some(PathBuf::from(dir).join("opencode"));
        }
        dirs::home_dir().map(|h| h.join(".config").join("opencode"))
    }

    fn is_installed(&self) -> bool {
        // `spawn_command` doesn't reference this path directly (opencode
        // auto-discovers plugin/*.js on its own), so a missing file doesn't
        // crash the launch the way pi's does -- but it does mean the session
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
    fn plugin_guards_on_agent_id_and_embeds_absolute_bin_path() {
        let a = make_adapter();
        let content = a.plugin_content();
        assert!(content.contains("process.env.OVERSEER_AGENT_ID"));
        assert!(content.contains("/usr/local/bin/overseer"));
    }

    #[test]
    fn plugin_maps_session_created_to_idle_for_root_running_for_child() {
        let content = make_adapter().plugin_content();
        assert!(content.contains(r#""session.created""#));
        assert!(content.contains(r#"OVERSEER_ROLE === "root""#));
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
        assert!(content.contains(r#""permission.ask""#));
        assert!(content.contains(r#""permission.replied""#));
    }

    #[test]
    fn plugin_never_touches_output_status_in_permission_ask() {
        // Overseer only observes the permission prompt; it must never
        // auto-resolve it (that decision stays the human's).
        let content = make_adapter().plugin_content();
        assert!(!content.contains("output.status"));
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
        assert!(ROOT_INSTRUCTIONS_CONTENT.contains("--adapter claude|opencode|pi"));
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

    // ── overseer_installed ────────────────────────────────────────────────────

    fn temp_config_dir() -> PathBuf {
        std::env::temp_dir().join(format!("overseer-opencode-installed-test-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn overseer_installed_false_on_a_fresh_config_dir() {
        let dir = temp_config_dir();
        let _env = crate::test_env::EnvGuard::set("XDG_CONFIG_HOME", dir.to_str().unwrap());
        // XDG_CONFIG_HOME joins "opencode" itself (`user_config_dir`) — assert
        // against the adapter's own resolved dir, not `dir` directly.
        assert!(!make_adapter().overseer_installed());
    }

    #[test]
    fn overseer_installed_false_with_only_the_plugin_written() {
        let xdg = temp_config_dir();
        let _env = crate::test_env::EnvGuard::set("XDG_CONFIG_HOME", xdg.to_str().unwrap());
        let a = make_adapter();
        let config_dir = a.user_config_dir().unwrap();
        let plugin = config_dir.join(PLUGIN_PATH);
        std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        std::fs::write(&plugin, "x").unwrap();
        assert!(!a.overseer_installed(), "instruction docs still missing");
        std::fs::remove_dir_all(&xdg).ok();
    }

    #[test]
    fn overseer_installed_true_once_plugin_and_both_instructions_exist_without_config_jsonc() {
        let xdg = temp_config_dir();
        let _env = crate::test_env::EnvGuard::set("XDG_CONFIG_HOME", xdg.to_str().unwrap());
        let a = make_adapter();
        let config_dir = a.user_config_dir().unwrap();
        for path in [PLUGIN_PATH, ROOT_INSTRUCTIONS_PATH, CHILD_INSTRUCTIONS_PATH] {
            let full = config_dir.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, "x").unwrap();
        }
        // opencode.jsonc (JsonArrayMerge) deliberately left unwritten — same
        // "can pre-exist for unrelated reasons" rationale as claude's settings.json.
        assert!(a.overseer_installed());
        std::fs::remove_dir_all(&xdg).ok();
    }
}
