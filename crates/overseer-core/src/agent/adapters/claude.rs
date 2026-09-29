use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use super::{AdapterCapabilities, AgentAdapter, CapabilitySupport, InstalledFile, LaunchContext, MergeStrategy};

const ROOT_SKILL_PATH: &str = "skills/overseer-root/SKILL.md";
const CHILD_SKILL_PATH: &str = "skills/overseer-child/SKILL.md";
const SETTINGS_PATH: &str = "settings.json";

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
        format!("'{}' {}", self.overseer_bin.to_string_lossy().replace('\'', "'\\''"), args)
    }

    fn settings_content(&self) -> String {
        let quiet_status = |args| format!(
            r#"[ -n "$OVERSEER_AGENT_ID" ] && [ -n "$OVERSEER_SOCKET" ] && {} >/dev/null || true"#,
            self.hook_command(args),
        );
        let running_cmd = quiet_status("status running --from-hook --clear-attention permission");
        let idle_cmd = quiet_status("status idle --from-hook --clear-attention permission");
        let blocked_cmd = quiet_status("status blocked --from-hook --attention permission");
        // SessionStart's own push additionally self-identifies as "claude" —
        // the only place this needs saying, since a bare-shell root's own
        // registered adapter is always the honest-but-uninformative "shell"
        // (`overseer start` never launches one). This is what an omitted
        // `--adapter` on a later `overseer spawn` from this session inherits
        // (`ipc::handlers`) — without it, a claude session running inside a
        // bare-shell root would never stop looking like "shell" to a spawn
        // default. Every other hook re-asserts `running`/`idle`/`blocked`
        // only — no need to repeat the adapter identity on every push.
        let session_start_running_cmd = self.hook_command("status running --from-hook --adapter claude --clear-context");
        let session_start_idle_cmd = self.hook_command("status idle --from-hook --adapter claude --clear-context");
        // Roots and taskless TUI-created children wait for a human prompt;
        // CLI-spawned children already have their initial task.
        let session_start_status_cmd = format!(
            r#"if [ -n "$OVERSEER_TASK" ]; then {session_start_running_cmd} >/dev/null; else {session_start_idle_cmd} >/dev/null; fi"#
        );
        // SessionStart stdout is context on startup, resume and compaction.
        // Only the daemon-verified role bootstrap reaches the model; status
        // acknowledgements stay quiet. Reference skills are optional.
        let context_cmd = self.hook_command("context");
        let session_start_cmd = format!(
            r#"[ -n "$OVERSEER_AGENT_ID" ] && [ -n "$OVERSEER_SOCKET" ] && {{ {session_start_status_cmd}; {context_cmd}; }} || true"#
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
                "Notification": [{
                    "matcher": "permission_prompt",
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
    fn capabilities(&self) -> AdapterCapabilities {
        // Live-probed with Claude Code 2.1.209. Lifecycle hooks and the
        // Notification(permission_prompt) payload fired with inherited identity.
        // Claude's authoritative 1M-aware context and rate-limit fields exist
        // only in the single, user-owned statusLine command slot; Overseer must
        // not replace or wrap arbitrary user configuration to observe them.
        AdapterCapabilities {
            lifecycle: CapabilitySupport::Supported,
            permission_requests: CapabilitySupport::Supported,
            provider_limits: CapabilitySupport::Unsupported {
                reason: "provider limits are exposed only to the user-owned statusLine command".to_string(),
            },
            context_usage: CapabilitySupport::Unsupported {
                reason: "authoritative context percentage is exposed only to the user-owned statusLine command".to_string(),
            },
        }
    }

    fn user_config_dir(&self) -> Option<PathBuf> {
        if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
            return Some(PathBuf::from(dir));
        }
        dirs::home_dir().map(|h| h.join(".claude"))
    }

    fn install_files(&self) -> Vec<InstalledFile> {
        vec![InstalledFile {
            path: PathBuf::from(SETTINGS_PATH),
            content: self.settings_content(),
            merge: MergeStrategy::JsonMerge,
        }]
    }

    fn legacy_paths(&self) -> Vec<PathBuf> {
        ["skills/overseer/SKILL.md", ROOT_SKILL_PATH, CHILD_SKILL_PATH]
            .into_iter().map(PathBuf::from).collect()
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
            command: "claude".to_string(),
            extra_args: vec![],
            task: "write unit tests for the login flow".to_string(),
            depth: 2,
        }
    }

    #[test]
    fn install_files_returns_only_settings() {
        let files = make_adapter().install_files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, Path::new(SETTINGS_PATH));
        assert!(matches!(files[0].merge, MergeStrategy::JsonMerge));
    }

    #[test]
    fn capabilities_match_live_probed_claude_2_1_209() {
        let capabilities = make_adapter().capabilities();
        assert_eq!(capabilities.lifecycle, CapabilitySupport::Supported);
        assert_eq!(capabilities.permission_requests, CapabilitySupport::Supported);
        assert!(matches!(capabilities.provider_limits, CapabilitySupport::Unsupported { .. }));
        assert!(matches!(capabilities.context_usage, CapabilitySupport::Unsupported { .. }));
    }

    #[test]
    fn migration_targets_only_owned_skill_files() {
        assert_eq!(make_adapter().legacy_paths(), vec![
            PathBuf::from("skills/overseer/SKILL.md"),
            PathBuf::from(ROOT_SKILL_PATH),
            PathBuf::from(CHILD_SKILL_PATH),
        ]);
    }

    #[test]
    fn settings_contains_post_tool_use_hook() {
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[0].content).unwrap();
        assert!(v["hooks"]["PostToolUse"].is_array());
        let cmd = v["hooks"]["PostToolUse"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("status running"));
    }

    #[test]
    fn settings_contains_user_prompt_submit_hook_pushing_running() {
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[0].content).unwrap();
        assert!(v["hooks"]["UserPromptSubmit"].is_array());
        let cmd = v["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("status running"));
    }

    #[test]
    fn settings_stop_hook_pushes_idle_not_done() {
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[0].content).unwrap();
        assert!(v["hooks"]["Stop"].is_array());
        let cmd = v["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("status idle"), "Stop must push idle, not done: {cmd}");
        assert!(!cmd.contains("status done"));
    }

    #[test]
    fn settings_contains_notification_hook_pushing_blocked() {
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[0].content).unwrap();
        assert!(v["hooks"]["Notification"].is_array());
        let cmd = v["hooks"]["Notification"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("status blocked"));
        assert!(cmd.contains("--attention permission"));
        assert_eq!(v["hooks"]["Notification"][0]["matcher"], "permission_prompt");
    }

    #[test]
    fn settings_session_start_uses_task_presence_for_initial_status() {
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[0].content).unwrap();
        let cmd = v["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("status idle"), "SessionStart should push idle for a root: {cmd}");
        assert!(cmd.contains("status running"), "SessionStart should push running for a child: {cmd}");
        assert!(cmd.contains(r#"-n "$OVERSEER_TASK""#), "must branch on OVERSEER_TASK: {cmd}");
        assert!(cmd.contains("OVERSEER_AGENT_ID"), "must stay guarded, no-op outside Overseer");
    }

    #[test]
    fn settings_session_start_self_identifies_as_claude() {
        // The only place this needs saying — a bare-shell root's registered
        // adapter is always "shell" until the real harness inside it says
        // otherwise; this is what an omitted `--adapter` on a later
        // `overseer spawn` inherits from.
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[0].content).unwrap();
        let cmd = v["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("--adapter claude"), "SessionStart should self-identify: {cmd}");
    }

    #[test]
    fn session_start_emits_only_role_context_from_quoted_binary() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("overseer-hook-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("overseer's executable");
        std::fs::write(&bin, "#!/bin/sh\nif [ \"$1\" = context ]; then printf 'role context\\n'; else printf 'status response\\n'; fi\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        let adapter = ClaudeAdapter::with_bin(bin);
        let settings: serde_json::Value = serde_json::from_str(&adapter.settings_content()).unwrap();
        let command = settings["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str().unwrap();
        for task in ["", "assigned work"] {
            let output = Command::new("/bin/sh").args(["-c", command])
                .env("OVERSEER_AGENT_ID", "agent").env("OVERSEER_SOCKET", "/tmp/test.sock")
                .env("OVERSEER_TASK", task).output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            assert_eq!(String::from_utf8_lossy(&output.stdout), "role context\n");
        }
        let output = Command::new("/bin/sh").args(["-c", command])
            .env_remove("OVERSEER_AGENT_ID").env_remove("OVERSEER_SOCKET").output().unwrap();
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn other_hooks_do_not_repeat_the_adapter_self_id() {
        // Only SessionStart needs to self-identify; every other push stays
        // exactly the plain status command it always was.
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[0].content).unwrap();
        for event in ["UserPromptSubmit", "PostToolUse", "Stop", "Notification"] {
            let cmd = v["hooks"][event][0]["hooks"][0]["command"].as_str().unwrap();
            assert!(!cmd.contains("--adapter"), "{event} should not repeat the adapter self-id: {cmd}");
        }
    }

    #[test]
    fn settings_hook_commands_use_absolute_path() {
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[0].content).unwrap();
        for event in ["PostToolUse", "UserPromptSubmit", "Stop", "Notification"] {
            let cmd = v["hooks"][event][0]["hooks"][0]["command"].as_str().unwrap();
            assert!(cmd.contains("'/usr/local/bin/overseer'"), "{event} hook must quote its absolute path: {cmd}");
        }
    }

    #[test]
    fn settings_hook_commands_pass_from_hook_flag() {
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[0].content).unwrap();
        for event in ["PostToolUse", "UserPromptSubmit", "Stop", "Notification"] {
            let cmd = v["hooks"][event][0]["hooks"][0]["command"].as_str().unwrap();
            assert!(cmd.contains("--from-hook"), "{event} hook must pass --from-hook, got: {cmd}");
        }
    }

    #[test]
    fn settings_hook_commands_never_pass_an_explicit_branch() {
        // `--from-hook` alone is enough: `cli::detect_current_branch` runs a
        // `git rev-parse --abbrev-ref HEAD` in the hook subprocess's own
        // cwd unconditionally (verified live to match Claude Code's own
        // tracked session cwd), so no hook command needs its own git
        // shelling or an explicit `--branch`.
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[0].content).unwrap();
        for event in ["PostToolUse", "UserPromptSubmit", "Stop", "Notification", "SessionStart"] {
            let cmd = v["hooks"][event][0]["hooks"][0]["command"].as_str().unwrap();
            assert!(!cmd.contains("--branch"), "{event} hook must not pass --branch, got: {cmd}");
        }
    }

    #[test]
    fn settings_entries_are_marked_overseer_managed() {
        let a = make_adapter();
        let v: serde_json::Value = serde_json::from_str(&a.install_files()[0].content).unwrap();
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
