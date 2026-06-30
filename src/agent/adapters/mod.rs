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
    pub task: String,
    /// Adapter binary name, e.g. "claude". Sourced from config, not hardcoded.
    pub command: String,
    pub extra_args: Vec<String>,
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
