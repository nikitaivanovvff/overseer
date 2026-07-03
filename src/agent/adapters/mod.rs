use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use crate::agent::{AgentId, AgentRole};

pub mod claude;

/// Identity passed to an adapter at launch time.
/// No filesystem/worktree paths — Overseer does not own a workspace.
/// `cwd` is where tmux starts the session (the repo root).
pub struct LaunchContext {
    pub agent_id: AgentId,
    pub role: AgentRole,
    pub parent_id: Option<AgentId>,
    pub socket: PathBuf,
    pub cwd: PathBuf,
    pub repo: String,
    /// Adapter binary name, e.g. "claude". Sourced from config, not hardcoded.
    pub command: String,
    pub extra_args: Vec<String>,
}

impl LaunchContext {
    pub fn identity(&self) -> AgentIdentity<'_> {
        AgentIdentity {
            agent_id: &self.agent_id,
            role: &self.role,
            parent_id: self.parent_id.as_ref(),
            socket: &self.socket,
            repo: &self.repo,
        }
    }
}

/// The subset of identity fields every launched session needs, regardless of
/// adapter. A borrowed view so callers that never build a full `LaunchContext`
/// (e.g. the bare-shell root path, which has no task/command/extra_args) can
/// still produce identical env vars without fabricating dummy values.
pub struct AgentIdentity<'a> {
    pub agent_id: &'a AgentId,
    pub role: &'a AgentRole,
    pub parent_id: Option<&'a AgentId>,
    pub socket: &'a std::path::Path,
    pub repo: &'a str,
}

/// Pure. Builds the OVERSEER_* env vars every launched session gets
/// (AGENTS.md "Agent Awareness"), independent of which adapter (if any) launched it.
pub fn identity_env(id: &AgentIdentity) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("OVERSEER_SOCKET".to_string(), id.socket.to_string_lossy().to_string());
    env.insert("OVERSEER_AGENT_ID".to_string(), id.agent_id.0.to_string());
    env.insert(
        "OVERSEER_ROLE".to_string(),
        match id.role {
            AgentRole::Root => "root".to_string(),
            AgentRole::Child => "child".to_string(),
        },
    );
    if let Some(parent_id) = id.parent_id {
        env.insert("OVERSEER_PARENT_ID".to_string(), parent_id.0.to_string());
    }
    env.insert("OVERSEER_REPO".to_string(), id.repo.to_string());
    env
}

/// A file written at the user level by `overseer teach`.
/// `path` is relative to the adapter's user config dir (e.g. "skills/overseer/SKILL.md").
pub struct InstalledFile {
    pub path: PathBuf,
    pub content: String,
    pub merge: MergeStrategy,
}

pub enum MergeStrategy {
    /// Overseer owns the file — write it verbatim.
    Overwrite,
    /// Merge the content into an existing JSON file, preserving unrelated keys.
    JsonMerge,
}

pub trait AgentAdapter: Send + Sync {
    #[allow(dead_code)]
    fn name(&self) -> &str;

    // --- teach (install-time, pure) ---

    /// User config dir for this agent (e.g. `~/.claude`). Resolved at call time.
    fn user_config_dir(&self) -> Option<PathBuf>;

    /// Files to install at the user level. Pure — no I/O.
    fn teach_files(&self) -> Vec<InstalledFile>;

    // --- launch (runtime, pure) ---

    /// Returns the command to run inside the tmux session (program + args; no cwd/env).
    fn spawn_command(&self, ctx: &LaunchContext) -> Command;

    /// Returns env vars to inject into the tmux session.
    fn env_inject(&self, ctx: &LaunchContext) -> HashMap<String, String>;
}

/// Returns the adapter for the given name, or `None` if unknown.
pub fn adapter_for(name: &str) -> Option<Box<dyn AgentAdapter>> {
    match name {
        "claude" => Some(Box::new(claude::ClaudeAdapter::new())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn root_identity<'a>(agent_id: &'a AgentId, socket: &'a Path, repo: &'a str) -> AgentIdentity<'a> {
        AgentIdentity { agent_id, role: &AgentRole::Root, parent_id: None, socket, repo }
    }

    #[test]
    fn identity_env_root_has_required_vars_no_parent() {
        let id = AgentId::new();
        let socket = PathBuf::from("/tmp/overseer.sock");
        let env = identity_env(&root_identity(&id, &socket, "myrepo"));
        assert_eq!(env.get("OVERSEER_AGENT_ID"), Some(&id.0.to_string()));
        assert_eq!(env.get("OVERSEER_SOCKET"), Some(&"/tmp/overseer.sock".to_string()));
        assert_eq!(env.get("OVERSEER_ROLE").map(String::as_str), Some("root"));
        assert_eq!(env.get("OVERSEER_REPO").map(String::as_str), Some("myrepo"));
        assert!(!env.contains_key("OVERSEER_PARENT_ID"));
    }

    #[test]
    fn identity_env_child_includes_parent_id() {
        let id = AgentId::new();
        let parent = AgentId::new();
        let socket = PathBuf::from("/tmp/overseer.sock");
        let identity = AgentIdentity {
            agent_id: &id,
            role: &AgentRole::Child,
            parent_id: Some(&parent),
            socket: &socket,
            repo: "myrepo",
        };
        let env = identity_env(&identity);
        assert_eq!(env.get("OVERSEER_ROLE").map(String::as_str), Some("child"));
        assert_eq!(env.get("OVERSEER_PARENT_ID"), Some(&parent.0.to_string()));
    }

    #[test]
    fn launch_context_identity_borrows_matching_fields() {
        let ctx = LaunchContext {
            agent_id: AgentId::new(),
            role: AgentRole::Root,
            parent_id: None,
            socket: PathBuf::from("/tmp/overseer.sock"),
            cwd: PathBuf::from("/tmp"),
            repo: "myrepo".to_string(),
            command: "claude".to_string(),
            extra_args: vec![],
        };
        let env = identity_env(&ctx.identity());
        assert_eq!(env.get("OVERSEER_AGENT_ID"), Some(&ctx.agent_id.0.to_string()));
        assert_eq!(env.get("OVERSEER_REPO").map(String::as_str), Some("myrepo"));
    }
}
