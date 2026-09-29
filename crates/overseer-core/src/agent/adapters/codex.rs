//! Experimental Codex CLI integration. Launch syntax checked against 0.159.0;
//! hook payloads follow https://learn.chatgpt.com/docs/hooks. No live model probe
//! has been run, so lifecycle support deliberately remains experimental.
use std::{collections::HashMap, path::PathBuf, process::Command};

use serde_json::{json, Value};

use super::{
    AdapterCapabilities, AgentAdapter, CapabilitySupport, InstalledFile, LaunchContext,
    MergeStrategy,
};
use crate::agent::AgentStatus;

/// A shell comment gives our groups an ownership marker without adding fields
/// outside Codex's hook schema. See settings::is_overseer_entry.
pub(crate) const HOOK_MARKER: &str = " # overseer-managed-codex-hook";

pub struct CodexAdapter {
    overseer_bin: PathBuf,
}

impl CodexAdapter {
    pub fn new() -> Self {
        Self {
            overseer_bin: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("overseer")),
        }
    }
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentAdapter for CodexAdapter {
    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            lifecycle: CapabilitySupport::Experimental {
                note: "Codex 0.159.0 hook contract; review hooks with /hooks and restart; live lifecycle probe pending".into(),
            },
            permission_requests: CapabilitySupport::Unsupported {
                reason: "permission hook attribution to main versus native subagent is not verified".into(),
            },
            provider_limits: CapabilitySupport::Unsupported {
                reason: "no verified structured provider-limit hook".into(),
            },
            context_usage: CapabilitySupport::Unsupported {
                reason: "no authoritative active context-window usage hook".into(),
            },
        }
    }

    fn user_config_dir(&self) -> Option<PathBuf> {
        std::env::var_os("CODEX_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
    }

    fn install_files(&self) -> Vec<InstalledFile> {
        let bin = self.overseer_bin.to_string_lossy().replace('\'', "'\"'\"'");
        let command = format!("if [ -n \"$OVERSEER_AGENT_ID\" ]; then '{bin}' codex-hook; else printf '{{}}'; fi{HOOK_MARKER}");
        let mut hooks = serde_json::Map::new();
        for event in ["SessionStart", "UserPromptSubmit", "Stop", "Interrupt"] {
            hooks.insert(
                event.into(),
                json!([{"hooks": [{"type": "command", "command": command, "timeout": 3}]}]),
            );
        }
        vec![InstalledFile {
            path: "hooks.json".into(),
            content: json!({"hooks": hooks}).to_string(),
            merge: MergeStrategy::JsonMerge,
        }]
    }

    fn is_installed(&self) -> bool {
        self.user_config_dir()
            .and_then(|dir| std::fs::read_to_string(dir.join("hooks.json")).ok())
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .is_some_and(|config| {
                ["SessionStart", "UserPromptSubmit", "Stop", "Interrupt"]
                    .iter()
                    .all(|event| {
                        config["hooks"][event].as_array().is_some_and(|groups| {
                            groups.iter().any(|group| {
                                group["hooks"].as_array().is_some_and(|hooks| {
                                    hooks.iter().any(|hook| {
                                        hook["command"]
                                            .as_str()
                                            .is_some_and(|command| command.ends_with(HOOK_MARKER))
                                    })
                                })
                            })
                        })
                    })
            })
    }

    fn spawn_command(&self, ctx: &LaunchContext) -> Command {
        let mut command = Command::new(&ctx.command);
        command.args(&ctx.extra_args);
        // A shared Codex server can inherit another session's identity and
        // outlive our PTY. Keep this session owned by Overseer's process tree.
        command.arg("--no-daemon");
        if !ctx.task.is_empty() {
            command.arg("--").arg(&ctx.task);
        }
        command
    }

    fn env_inject(&self, ctx: &LaunchContext) -> HashMap<String, String> {
        let mut env = super::identity_env(&ctx.identity());
        if !ctx.task.is_empty() {
            env.insert("OVERSEER_TASK".into(), ctx.task.clone());
        }
        env
    }
}

#[derive(Debug, PartialEq)]
pub struct CodexHookUpdate {
    pub status: AgentStatus,
    pub bootstrap: bool,
    pub model_name: Option<String>,
}

/// Only the explicit main-session lifecycle is mapped. Native subagents share
/// their parent's session_id, so that field cannot establish independent
/// Overseer identity. Tool and permission events are intentionally not mapped.
pub fn normalize_hook(payload: &Value, has_task: bool) -> Option<CodexHookUpdate> {
    if payload.get("agent_id").is_some() || payload.get("agent_type").is_some() {
        return None;
    }
    let event = payload.get("hook_event_name")?.as_str()?;
    let (status, bootstrap) = match event {
        "SessionStart" => {
            let source = payload.get("source")?.as_str()?;
            let active = match source {
                "compact" => true,
                "startup" => has_task,
                "resume" | "clear" => false,
                _ => return None,
            };
            (
                if active {
                    AgentStatus::Running
                } else {
                    AgentStatus::Idle
                },
                true,
            )
        }
        "UserPromptSubmit" => (AgentStatus::Running, false),
        "Stop" | "Interrupt" => (AgentStatus::Idle, false),
        _ => return None,
    };
    Some(CodexHookUpdate {
        status,
        bootstrap,
        model_name: payload
            .get("model")
            .and_then(Value::as_str)
            .filter(|model| !model.trim().is_empty())
            .map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentId, AgentRole};

    #[test]
    fn launches_interactively_with_owned_process_and_literal_prompt() {
        let mut ctx = LaunchContext {
            agent_id: AgentId::new(),
            role: AgentRole::Child,
            parent_id: Some(AgentId::new()),
            socket: "/tmp/overseer.sock".into(),
            cwd: "/tmp".into(),
            repo: "repo".into(),
            command: "/custom/codex".into(),
            extra_args: vec!["--model".into(), "example".into()],
            task: "--task begins with dash".into(),
            depth: 2,
        };
        let adapter = CodexAdapter::new();
        let command = adapter.spawn_command(&ctx);
        assert_eq!(command.get_program(), "/custom/codex");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "--model",
                "example",
                "--no-daemon",
                "--",
                "--task begins with dash"
            ]
        );
        assert_eq!(adapter.env_inject(&ctx)["OVERSEER_TASK"], ctx.task);
        ctx.task.clear();
        assert_eq!(adapter.spawn_command(&ctx).get_args().count(), 3);
        assert!(!adapter.env_inject(&ctx).contains_key("OVERSEER_TASK"));
    }

    #[test]
    fn hook_contract_does_not_infer_completion_or_native_subagent_state() {
        for (event, expected) in [
            ("UserPromptSubmit", AgentStatus::Running),
            ("Stop", AgentStatus::Idle),
            ("Interrupt", AgentStatus::Idle),
        ] {
            let mut payload = json!({"hook_event_name": event, "model": "verified-model"});
            let update = normalize_hook(&payload, true).unwrap();
            assert_eq!(update.status, expected);
            assert_eq!(update.model_name.as_deref(), Some("verified-model"));
            assert!(!update.bootstrap);
            payload["agent_id"] = json!("native-child");
            assert!(normalize_hook(&payload, true).is_none());
        }
        for event in [
            "SubagentStart",
            "SubagentStop",
            "PermissionRequest",
            "PostToolUse",
            "SessionEnd",
        ] {
            assert!(normalize_hook(&json!({"hook_event_name": event}), false).is_none());
        }
    }

    #[test]
    fn bootstrap_status_distinguishes_initial_tasks_and_compaction() {
        let mut payload = json!({"hook_event_name": "SessionStart", "source": "startup"});
        assert_eq!(
            normalize_hook(&payload, false).unwrap().status,
            AgentStatus::Idle
        );
        assert_eq!(
            normalize_hook(&payload, true).unwrap().status,
            AgentStatus::Running
        );
        payload["source"] = json!("compact");
        let update = normalize_hook(&payload, false).unwrap();
        assert_eq!(update.status, AgentStatus::Running);
        assert!(update.bootstrap);
        payload["source"] = json!("unknown");
        assert!(normalize_hook(&payload, true).is_none());
    }

    #[test]
    fn installation_preserves_hook_trust_and_uses_absolute_escaped_binary() {
        let adapter = CodexAdapter {
            overseer_bin: "/opt/it's overseer/bin/overseer".into(),
        };
        let files = adapter.install_files();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, PathBuf::from("hooks.json"));
        let config: Value = serde_json::from_str(&files[0].content).unwrap();
        let command = config["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(command.contains("/opt/it'\"'\"'s overseer/bin/overseer"));
        assert!(command.contains("$OVERSEER_AGENT_ID"));
        assert!(command.ends_with(HOOK_MARKER));
        assert!(!files[0].content.contains("dangerously"));
        assert!(matches!(
            adapter.capabilities().lifecycle,
            CapabilitySupport::Experimental { .. }
        ));
    }
}
