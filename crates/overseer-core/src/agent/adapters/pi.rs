use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use crate::agent::AgentRole;

use super::{AgentAdapter, InstalledFile, LaunchContext, MergeStrategy};

const EXTENSION_PATH: &str = "extensions/overseer.ts";
const ROOT_INSTRUCTIONS_PATH: &str = "overseer-root.md";
const CHILD_INSTRUCTIONS_PATH: &str = "overseer-child.md";

const ROOT_INSTRUCTIONS_CONTENT: &str = include_str!("pi_root.md");
const CHILD_INSTRUCTIONS_CONTENT: &str = include_str!("pi_child.md");

pub struct PiAdapter {
    overseer_bin: PathBuf,
}

impl PiAdapter {
    pub fn new() -> Self {
        let overseer_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("overseer"));
        Self { overseer_bin }
    }

    #[cfg(test)]
    pub fn with_bin(overseer_bin: PathBuf) -> Self {
        Self { overseer_bin }
    }

    fn config_dir(&self) -> PathBuf {
        if let Ok(dir) = std::env::var("PI_CODING_AGENT_DIR") {
            return PathBuf::from(dir);
        }
        dirs::home_dir().map(|h| h.join(".pi").join("agent")).unwrap_or_else(|| PathBuf::from("."))
    }

    fn extension_full_path(&self) -> PathBuf {
        self.config_dir().join(EXTENSION_PATH)
    }

    fn instructions_full_path(&self, role: &AgentRole) -> PathBuf {
        let name = match role {
            AgentRole::Root => ROOT_INSTRUCTIONS_PATH,
            AgentRole::Child => CHILD_INSTRUCTIONS_PATH,
        };
        self.config_dir().join(name)
    }

    /// Loaded via `pi --extension <path>` at spawn time (verified live,
    /// HARNESSES.md Task 0) rather than pi's package-manager (`pi install
    /// <source>`, which registers into `settings.json` and is meant for
    /// shareable packages, not a one-file first-party hook) — this file still
    /// lives under pi's own config dir so `overseer install`/`--uninstall`
    /// has one obvious, owned path to write and remove, but nothing needs to
    /// touch `settings.json` at all.
    ///
    /// Event mapping verified against the installed
    /// `@earendil-works/pi-coding-agent` TypeScript definitions and a live
    /// session (HARNESSES.md Task 0): `session_start` → running (mirrors
    /// Claude's `SessionStart` hook), `agent_start` → running, `agent_end` →
    /// idle, `session_shutdown` → nothing (exit watcher owns error). No
    /// permission event exists in pi's `ExtensionEvent` union at all — pi has
    /// no built-in permission-prompt concept (confirmed, not just per the
    /// original spec's caveat: permission gates are themselves extensions a
    /// user would have to separately install), so this adapter never pushes
    /// `blocked`.
    fn extension_content(&self) -> String {
        let bin = serde_json::to_string(&self.overseer_bin.to_string_lossy().to_string())
            .unwrap_or_else(|_| "\"overseer\"".to_string());
        format!(
            r#"import {{ execFile }} from "node:child_process";

const OVERSEER_BIN = {bin};

export default function (pi) {{
  if (!process.env.OVERSEER_AGENT_ID) {{
    return;
  }}
  const push = (status) => execFile(OVERSEER_BIN, ["status", status], () => {{}});

  // Roots and taskless TUI-created children wait for a human prompt;
  // CLI-spawned children already have their initial task.
  pi.on("session_start", () => {{
    const initial = process.env.OVERSEER_TASK ? "running" : "idle";
    execFile(OVERSEER_BIN, ["status", initial, "--adapter", "pi"], () => {{}});
  }});
  pi.on("agent_start", () => push("running"));
  pi.on("agent_end", () => push("idle"));
  // session_shutdown: nothing -- the exit watcher owns error, not a lifecycle push.
}}
"#
        )
    }
}

impl Default for PiAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentAdapter for PiAdapter {
    fn user_config_dir(&self) -> Option<PathBuf> {
        Some(self.config_dir())
    }

    fn is_installed(&self) -> bool {
        // `spawn_command` unconditionally passes `--extension
        // <this path>`; pi hard-errors and exits if it doesn't exist
        // (verified live, HARNESSES.md), so a missing file here means the
        // launch itself would fail, not just run without status reporting.
        self.extension_full_path().exists()
    }

    fn install_files(&self) -> Vec<InstalledFile> {
        vec![
            InstalledFile {
                path: PathBuf::from(EXTENSION_PATH),
                content: self.extension_content(),
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
        ]
    }

    fn spawn_command(&self, ctx: &LaunchContext) -> Command {
        let mut cmd = Command::new(&ctx.command);
        for arg in &ctx.extra_args {
            cmd.arg(arg);
        }
        cmd.arg("--extension").arg(self.extension_full_path());
        // Verified live (HARNESSES.md Task 0): `--append-system-prompt <path>`
        // reads the file's contents and appends them when the argument is an
        // existing path (falls back to literal text otherwise) -- the role
        // split happens here, per-invocation, rather than by loading both
        // docs unconditionally the way opencode's shared instructions array
        // has to.
        cmd.arg("--append-system-prompt").arg(self.instructions_full_path(&ctx.role));
        // Verified live: a positional message stays interactive (unlike
        // `--print`/`-p`, which is explicitly one-shot).
        if !ctx.task.is_empty() {
            cmd.arg(&ctx.task);
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
    use crate::agent::AgentId;

    fn make_adapter() -> PiAdapter {
        PiAdapter::with_bin(PathBuf::from("/usr/local/bin/overseer"))
    }

    fn make_root_ctx() -> LaunchContext {
        LaunchContext {
            agent_id: AgentId::new(),
            role: AgentRole::Root,
            parent_id: None,
            socket: PathBuf::from("/tmp/overseer.sock"),
            cwd: PathBuf::from("/projects/myrepo"),
            repo: "myrepo".to_string(),
            command: "pi".to_string(),
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
            command: "pi".to_string(),
            extra_args: vec![],
            task: "write unit tests for the login flow".to_string(),
            depth: 2,
        }
    }

    #[test]
    fn install_files_returns_extension_and_two_instructions() {
        let a = make_adapter();
        let files = a.install_files();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, PathBuf::from(EXTENSION_PATH));
        assert!(matches!(files[0].merge, MergeStrategy::Overwrite));
        assert_eq!(files[1].path, PathBuf::from(ROOT_INSTRUCTIONS_PATH));
        assert_eq!(files[2].path, PathBuf::from(CHILD_INSTRUCTIONS_PATH));
    }

    #[test]
    fn extension_guards_on_agent_id_and_embeds_absolute_bin_path() {
        let content = make_adapter().extension_content();
        assert!(content.contains("process.env.OVERSEER_AGENT_ID"));
        assert!(content.contains("/usr/local/bin/overseer"));
    }

    #[test]
    fn extension_maps_session_and_agent_lifecycle_events() {
        let content = make_adapter().extension_content();
        assert!(content.contains(r#""session_start""#));
        assert!(content.contains(r#""agent_start""#));
        assert!(content.contains(r#""agent_end""#));
    }

    #[test]
    fn extension_session_start_status_uses_task_presence() {
        let content = make_adapter().extension_content();
        assert!(content.contains("process.env.OVERSEER_TASK"));
        assert!(content.contains(r#""idle""#));
        assert!(content.contains(r#""running""#));
    }

    #[test]
    fn extension_self_identifies_as_pi_only_on_session_start() {
        // The only place this needs saying — a bare-shell root's registered
        // adapter is always "shell" until the real harness inside it says
        // otherwise; this is what an omitted --adapter on a later
        // `overseer spawn` inherits.
        let content = make_adapter().extension_content();
        assert!(content.contains(r#""--adapter", "pi""#));
        let adapter_occurrences = content.matches("--adapter").count();
        assert_eq!(adapter_occurrences, 1, "adapter self-id should appear exactly once: {content}");
    }

    #[test]
    fn extension_never_pushes_blocked() {
        // No permission event exists in pi's ExtensionEvent union at all
        // (verified against the installed types) -- pushing "blocked" from
        // this extension would be fabricating a signal pi never gives us.
        let content = make_adapter().extension_content();
        assert!(!content.contains("\"blocked\""));
    }

    #[test]
    fn root_instructions_bless_cross_harness_spawn_and_document_the_blocked_caveat() {
        assert!(ROOT_INSTRUCTIONS_CONTENT.contains("--adapter claude|opencode|pi"));
        assert!(ROOT_INSTRUCTIONS_CONTENT.to_lowercase().contains("no built-in permission"));
    }

    #[test]
    fn root_instructions_forbid_the_built_in_subagent_tool_for_delegation() {
        // A real user reported the model using its own Task/subagent tool
        // instead of `overseer spawn` — those subagents are invisible to
        // Overseer entirely (no tree row, no tracking).
        assert!(ROOT_INSTRUCTIONS_CONTENT.to_lowercase().contains("do not use your own built-in subagent"));
    }

    #[test]
    fn child_instructions_document_done_status() {
        assert!(CHILD_INSTRUCTIONS_CONTENT.contains("overseer status done"));
    }

    #[test]
    fn child_instructions_require_visible_delegation_and_document_depth_three() {
        assert!(CHILD_INSTRUCTIONS_CONTENT.contains("never your harness's built-in"));
        assert!(CHILD_INSTRUCTIONS_CONTENT.contains("read-only lookup"));
        assert!(CHILD_INSTRUCTIONS_CONTENT.contains("OVERSEER_DEPTH"));
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
    fn spawn_command_always_passes_the_extension_flag() {
        let a = make_adapter();
        let cmd = a.spawn_command(&make_root_ctx());
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert_eq!(args[0], "--extension");
        assert!(args[1].ends_with("extensions/overseer.ts"));
    }

    #[test]
    fn spawn_command_picks_root_instructions_for_a_root() {
        let a = make_adapter();
        let cmd = a.spawn_command(&make_root_ctx());
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        let idx = args.iter().position(|a| a == "--append-system-prompt").unwrap();
        assert!(args[idx + 1].ends_with(ROOT_INSTRUCTIONS_PATH));
    }

    #[test]
    fn spawn_command_picks_child_instructions_for_a_child() {
        let a = make_adapter();
        let cmd = a.spawn_command(&make_child_ctx(AgentId::new()));
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        let idx = args.iter().position(|a| a == "--append-system-prompt").unwrap();
        assert!(args[idx + 1].ends_with(CHILD_INSTRUCTIONS_PATH));
    }

    #[test]
    fn spawn_command_appends_task_as_final_positional_arg() {
        let a = make_adapter();
        let cmd = a.spawn_command(&make_child_ctx(AgentId::new()));
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert_eq!(args.last().unwrap(), "write unit tests for the login flow");
    }

    #[test]
    fn spawn_command_empty_task_appends_no_positional() {
        let a = make_adapter();
        let cmd = a.spawn_command(&make_root_ctx());
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        // Exactly the two flag pairs, nothing trailing.
        assert_eq!(args.len(), 4);
    }

    #[test]
    fn env_inject_child_includes_overseer_task() {
        let a = make_adapter();
        let env = a.env_inject(&make_child_ctx(AgentId::new()));
        assert_eq!(env.get("OVERSEER_TASK").map(String::as_str), Some("write unit tests for the login flow"));
    }

    #[test]
    fn env_inject_root_has_no_overseer_task() {
        let a = make_adapter();
        let env = a.env_inject(&make_root_ctx());
        assert!(!env.contains_key("OVERSEER_TASK"));
    }

}
