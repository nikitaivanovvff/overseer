use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use super::{AgentAdapter, InstalledFile, LaunchContext, MergeStrategy};

const ROOT_SKILL_PATH: &str = "skills/overseer-root/SKILL.md";
const CHILD_SKILL_PATH: &str = "skills/overseer-child/SKILL.md";
const SETTINGS_PATH: &str = "settings.json";

/// The old single-skill layout, superseded by the root/child split above —
/// deleted on install/uninstall so a stale copy doesn't keep pointing agents
/// at content that no longer matches the (now role-specific) hook behavior.
const LEGACY_SKILL_DIR: &str = "skills/overseer";

const ROOT_SKILL_CONTENT: &str = include_str!("overseer_root_skill.md");
const CHILD_SKILL_CONTENT: &str = include_str!("overseer_child_skill.md");

pub struct ClaudeAdapter {
    overseer_bin: PathBuf,
}

impl ClaudeAdapter {
    pub fn new() -> Self {
        let overseer_bin = std::env::current_exe()
            .unwrap_or_else(|_| PathBuf::from("overseer"));
        Self { overseer_bin }
    }

    #[cfg(test)]
    pub fn with_bin(overseer_bin: PathBuf) -> Self {
        Self { overseer_bin }
    }

    fn hook_command(&self, args: &str) -> String {
        format!("{} {}", self.overseer_bin.display(), args)
    }

    fn settings_content(&self) -> String {
        let running_cmd = self.hook_command("status running --from-hook");
        let idle_cmd = self.hook_command("status idle --from-hook");
        let blocked_cmd = self.hook_command("status blocked --from-hook");
        // SessionStart's own push additionally self-identifies as "claude" —
        // the only place this needs saying, since a bare-shell root's own
        // registered adapter is always the honest-but-uninformative "shell"
        // (`overseer start` never launches one). This is what an omitted
        // `--adapter` on a later `overseer spawn` from this session inherits
        // (`ipc::handlers`) — without it, a claude session running inside a
        // bare-shell root would never stop looking like "shell" to a spawn
        // default. Every other hook re-asserts `running`/`idle`/`blocked`
        // only — no need to repeat the adapter identity on every push.
        let session_start_status_cmd = self.hook_command("status running --from-hook --adapter claude");
        // Also pushes `running` immediately at session start (not just at the first
        // tool call) — closes the gap between "user runs claude" and "first tool
        // use" for a bare-shell root that started `Idle`. Still pure push, no polling.
        let session_start_cmd = format!(
            r#"[ -n "$OVERSEER_AGENT_ID" ] && {{ printf 'You are managed by Overseer (role: %s). Follow the overseer-%s skill.\n' "$OVERSEER_ROLE" "$OVERSEER_ROLE"; {session_start_status_cmd}; }} || true"#
        );

        serde_json::json!({
            "hooks": {
                "SessionStart": [{
                    "matcher": "",
                    "_overseer": true,
                    "hooks": [{"type": "command", "command": session_start_cmd}]
                }],
                "UserPromptSubmit": [{
                    "matcher": "",
                    "_overseer": true,
                    "hooks": [{"type": "command", "command": running_cmd.clone()}]
                }],
                "PostToolUse": [{
                    "matcher": "",
                    "_overseer": true,
                    "hooks": [{"type": "command", "command": running_cmd}]
                }],
                // Not `done` — the agent finished responding, not necessarily the
                // task. `done` is only reachable via an explicit push from the
                // agent itself (AGENTS.md "Status is push, not pull").
                "Stop": [{
                    "matcher": "",
                    "_overseer": true,
                    "hooks": [{"type": "command", "command": idle_cmd}]
                }],
                // Notification also fires for the 60s idle nag, which is not a
                // permission request — main.rs's --from-hook classification
                // downgrades that case back to `idle` so this doesn't lie.
                "Notification": [{
                    "matcher": "",
                    "_overseer": true,
                    "hooks": [{"type": "command", "command": blocked_cmd}]
                }]
            }
        })
        .to_string()
    }
}

impl Default for ClaudeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentAdapter for ClaudeAdapter {
    fn user_config_dir(&self) -> Option<PathBuf> {
        if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
            return Some(PathBuf::from(dir));
        }
        dirs::home_dir().map(|h| h.join(".claude"))
    }

    fn install_files(&self) -> Vec<InstalledFile> {
        vec![
            InstalledFile {
                path: PathBuf::from(ROOT_SKILL_PATH),
                content: ROOT_SKILL_CONTENT.to_string(),
                merge: MergeStrategy::Overwrite,
            },
            InstalledFile {
                path: PathBuf::from(CHILD_SKILL_PATH),
                content: CHILD_SKILL_CONTENT.to_string(),
                merge: MergeStrategy::Overwrite,
            },
            InstalledFile {
                path: PathBuf::from(SETTINGS_PATH),
                content: self.settings_content(),
                merge: MergeStrategy::JsonMerge,
            },
        ]
    }

    fn legacy_paths(&self) -> Vec<PathBuf> {
        vec![PathBuf::from(LEGACY_SKILL_DIR)]
    }

    fn spawn_command(&self, ctx: &LaunchContext) -> Command {
        let mut cmd = Command::new(&ctx.command);
        for arg in &ctx.extra_args {
            cmd.arg(arg);
        }
        // A non-empty task is the child's initial prompt: `std::process::Command`
        // passes it as one argv entry with no shell involved, so no quoting is
        // needed. Claude Code treats a positional arg as the starting prompt and
        // stays interactive — the session remains a normal steerable PTY.
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
    use crate::agent::{AgentId, AgentRole};
    use std::path::Path;

    fn make_adapter() -> ClaudeAdapter {
        ClaudeAdapter::with_bin(PathBuf::from("/usr/local/bin/overseer"))
    }

    fn make_root_ctx() -> LaunchContext {
        LaunchContext {
            agent_id: AgentId::new(),
            role: AgentRole::Root,
            parent_id: None,
            socket: PathBuf::from("/tmp/overseer.sock"),
            cwd: PathBuf::from("/projects/myrepo"),
            repo: "myrepo".to_string(),
            command: "claude".to_string(),
            extra_args: vec!["--some-flag".to_string()],
            task: String::new(),
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
            command: "claude".to_string(),
            extra_args: vec![],
            task: "write unit tests for the login flow".to_string(),
        }
    }

    #[test]
    fn install_files_returns_two_skills_and_settings() {
        let a = make_adapter();
        let files = a.install_files();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, Path::new(ROOT_SKILL_PATH));
        assert!(matches!(files[0].merge, MergeStrategy::Overwrite));
        assert_eq!(files[1].path, Path::new(CHILD_SKILL_PATH));
        assert!(matches!(files[1].merge, MergeStrategy::Overwrite));
        assert_eq!(files[2].path, Path::new(SETTINGS_PATH));
        assert!(matches!(files[2].merge, MergeStrategy::JsonMerge));
    }

    #[test]
    fn legacy_paths_targets_the_old_single_skill_dir() {
        let a = make_adapter();
        assert_eq!(a.legacy_paths(), vec![PathBuf::from("skills/overseer")]);
    }

    #[test]
    fn root_skill_content_is_not_empty_and_has_frontmatter() {
        assert!(!ROOT_SKILL_CONTENT.is_empty());
        assert!(ROOT_SKILL_CONTENT.contains("name: overseer-root"), "missing name frontmatter");
        assert!(ROOT_SKILL_CONTENT.contains("description:"), "missing description frontmatter");
    }

    #[test]
    fn child_skill_content_is_not_empty_and_has_frontmatter() {
        assert!(!CHILD_SKILL_CONTENT.is_empty());
        assert!(CHILD_SKILL_CONTENT.contains("name: overseer-child"), "missing name frontmatter");
        assert!(CHILD_SKILL_CONTENT.contains("description:"), "missing description frontmatter");
    }

    #[test]
    fn root_skill_documents_spawn_and_forbids_nesting() {
        assert!(ROOT_SKILL_CONTENT.contains("overseer spawn"));
        assert!(ROOT_SKILL_CONTENT.to_lowercase().contains("not spawn further")
            || ROOT_SKILL_CONTENT.contains("may not spawn further"));
    }

    #[test]
    fn root_skill_documents_the_name_flag_for_short_kebab_labels() {
        assert!(ROOT_SKILL_CONTENT.contains("--name"));
        assert!(ROOT_SKILL_CONTENT.contains("kebab-case"));
    }

    #[test]
    fn root_skill_documents_status_secs() {
        assert!(ROOT_SKILL_CONTENT.contains("status_secs"));
    }

    #[test]
    fn root_skill_blesses_cross_harness_spawn() {
        assert!(ROOT_SKILL_CONTENT.contains("--adapter claude|opencode|pi"));
    }

    #[test]
    fn child_skill_documents_overseer_task_and_done_status() {
        assert!(CHILD_SKILL_CONTENT.contains("OVERSEER_TASK"));
        assert!(CHILD_SKILL_CONTENT.contains("overseer status done"));
    }

    #[test]
    fn settings_contains_post_tool_use_hook() {
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[2].content).unwrap();
        assert!(v["hooks"]["PostToolUse"].is_array());
        let cmd = v["hooks"]["PostToolUse"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("status running"));
    }

    #[test]
    fn settings_contains_user_prompt_submit_hook_pushing_running() {
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[2].content).unwrap();
        assert!(v["hooks"]["UserPromptSubmit"].is_array());
        let cmd = v["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("status running"));
    }

    #[test]
    fn settings_stop_hook_pushes_idle_not_done() {
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[2].content).unwrap();
        assert!(v["hooks"]["Stop"].is_array());
        let cmd = v["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("status idle"), "Stop must push idle, not done: {cmd}");
        assert!(!cmd.contains("status done"));
    }

    #[test]
    fn settings_contains_notification_hook_pushing_blocked() {
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[2].content).unwrap();
        assert!(v["hooks"]["Notification"].is_array());
        let cmd = v["hooks"]["Notification"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("status blocked"));
    }

    #[test]
    fn settings_session_start_also_pushes_running() {
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[2].content).unwrap();
        let cmd = v["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("status running"), "SessionStart should also push running: {cmd}");
        assert!(cmd.contains("OVERSEER_AGENT_ID"), "must stay guarded, no-op outside Overseer");
    }

    #[test]
    fn settings_session_start_self_identifies_as_claude() {
        // The only place this needs saying — a bare-shell root's registered
        // adapter is always "shell" until the real harness inside it says
        // otherwise; this is what an omitted `--adapter` on a later
        // `overseer spawn` inherits from.
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[2].content).unwrap();
        let cmd = v["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("--adapter claude"), "SessionStart should self-identify: {cmd}");
    }

    #[test]
    fn other_hooks_do_not_repeat_the_adapter_self_id() {
        // Only SessionStart needs to self-identify; every other push stays
        // exactly the plain status command it always was.
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[2].content).unwrap();
        for event in ["UserPromptSubmit", "PostToolUse", "Stop", "Notification"] {
            let cmd = v["hooks"][event][0]["hooks"][0]["command"].as_str().unwrap();
            assert!(!cmd.contains("--adapter"), "{event} should not repeat the adapter self-id: {cmd}");
        }
    }

    #[test]
    fn settings_hook_commands_use_absolute_path() {
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[2].content).unwrap();
        for event in ["PostToolUse", "UserPromptSubmit", "Stop", "Notification"] {
            let cmd = v["hooks"][event][0]["hooks"][0]["command"].as_str().unwrap();
            assert!(cmd.starts_with('/'), "{event} hook must be absolute path, got: {cmd}");
        }
    }

    #[test]
    fn settings_hook_commands_pass_from_hook_flag() {
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[2].content).unwrap();
        for event in ["PostToolUse", "UserPromptSubmit", "Stop", "Notification"] {
            let cmd = v["hooks"][event][0]["hooks"][0]["command"].as_str().unwrap();
            assert!(cmd.contains("--from-hook"), "{event} hook must pass --from-hook, got: {cmd}");
        }
    }

    #[test]
    fn settings_entries_are_marked_overseer_managed() {
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[2].content).unwrap();
        for event in ["PostToolUse", "UserPromptSubmit", "Stop", "Notification", "SessionStart"] {
            assert_eq!(
                v["hooks"][event][0]["_overseer"].as_bool(),
                Some(true),
                "{event} entry missing _overseer sentinel"
            );
        }
    }

    #[test]
    fn env_inject_root_has_required_vars() {
        let a = make_adapter();
        let ctx = make_root_ctx();
        let env = a.env_inject(&ctx);
        assert!(env.contains_key("OVERSEER_SOCKET"));
        assert!(env.contains_key("OVERSEER_AGENT_ID"));
        assert_eq!(env.get("OVERSEER_ROLE").map(|s| s.as_str()), Some("root"));
        assert!(!env.contains_key("OVERSEER_PARENT_ID"));
        assert!(!env.contains_key("OVERSEER_BRANCH"));
    }

    #[test]
    fn env_inject_repo_is_repo_name_not_task() {
        let a = make_adapter();
        let ctx = make_root_ctx();
        let env = a.env_inject(&ctx);
        assert_eq!(env.get("OVERSEER_REPO").map(|s| s.as_str()), Some("myrepo"));
    }

    #[test]
    fn env_inject_child_includes_parent_id() {
        let a = make_adapter();
        let parent = AgentId::new();
        let parent_full = parent.0.to_string();
        let ctx = make_child_ctx(parent);
        let env = a.env_inject(&ctx);
        assert_eq!(env.get("OVERSEER_ROLE").map(|s| s.as_str()), Some("child"));
        assert_eq!(env.get("OVERSEER_PARENT_ID"), Some(&parent_full));
    }

    #[test]
    fn env_inject_agent_id_is_full_uuid() {
        let a = make_adapter();
        let ctx = make_root_ctx();
        let id_str = ctx.agent_id.0.to_string();
        let env = a.env_inject(&ctx);
        assert_eq!(env.get("OVERSEER_AGENT_ID"), Some(&id_str));
        assert_eq!(env["OVERSEER_AGENT_ID"].len(), 36);
    }

    #[test]
    fn spawn_command_uses_ctx_command_and_extra_args() {
        let a = make_adapter();
        let ctx = make_root_ctx();
        let cmd = a.spawn_command(&ctx);
        assert_eq!(cmd.get_program(), "claude");
        let args: Vec<_> = cmd.get_args().collect();
        assert!(args.iter().any(|a| *a == "--some-flag"));
    }

    #[test]
    fn spawn_command_empty_extra_args_launches_bare_claude() {
        let a = make_adapter();
        let mut ctx = make_root_ctx();
        ctx.extra_args = vec![];
        let cmd = a.spawn_command(&ctx);
        assert_eq!(cmd.get_program(), "claude");
        assert_eq!(cmd.get_args().count(), 0);
    }

    #[test]
    fn spawn_command_appends_task_as_final_positional_arg() {
        let a = make_adapter();
        let ctx = make_child_ctx(AgentId::new());
        let cmd = a.spawn_command(&ctx);
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(*args.last().unwrap(), "write unit tests for the login flow");
    }

    #[test]
    fn spawn_command_empty_task_appends_nothing() {
        let a = make_adapter();
        let ctx = make_root_ctx(); // root ctx always has an empty task
        let cmd = a.spawn_command(&ctx);
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert_eq!(args, vec!["--some-flag".to_string()]);
    }

    #[test]
    fn env_inject_child_includes_overseer_task() {
        let a = make_adapter();
        let ctx = make_child_ctx(AgentId::new());
        let env = a.env_inject(&ctx);
        assert_eq!(
            env.get("OVERSEER_TASK").map(String::as_str),
            Some("write unit tests for the login flow")
        );
    }

    #[test]
    fn env_inject_root_has_no_overseer_task() {
        let a = make_adapter();
        let ctx = make_root_ctx(); // root ctx always has an empty task
        let env = a.env_inject(&ctx);
        assert!(!env.contains_key("OVERSEER_TASK"));
    }
}
