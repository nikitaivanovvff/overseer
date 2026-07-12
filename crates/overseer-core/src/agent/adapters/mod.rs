use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use crate::agent::{AgentId, AgentRole};

pub mod claude;
pub mod opencode;
pub mod pi;

/// Identity passed to an adapter at launch time.
/// No filesystem/worktree paths — Overseer does not own a workspace.
/// `cwd` is where the PTY starts the session (the repo root).
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
    /// The child's initial prompt, verbatim (empty for a root — it has no task).
    /// The adapter appends this as the final positional arg to `spawn_command`
    /// and `env_inject` re-exposes it as `$OVERSEER_TASK` for the running agent.
    pub task: String,
    /// One-based position in the registry tree, derived at launch time.
    pub depth: usize,
}

impl LaunchContext {
    pub fn identity(&self) -> AgentIdentity<'_> {
        AgentIdentity {
            agent_id: &self.agent_id,
            role: &self.role,
            parent_id: self.parent_id.as_ref(),
            socket: &self.socket,
            repo: &self.repo,
            depth: self.depth,
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
    pub depth: usize,
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
    env.insert("OVERSEER_DEPTH".to_string(), id.depth.to_string());
    env
}

/// A file written at the user level by `overseer install`.
/// `path` is relative to the adapter's user config dir (e.g. "skills/overseer-root/SKILL.md").
pub struct InstalledFile {
    pub path: PathBuf,
    pub content: String,
    pub merge: MergeStrategy,
}

pub enum MergeStrategy {
    /// Overseer owns the file — write it verbatim.
    Overwrite,
    /// Merge the content into an existing JSON file, preserving unrelated keys.
    /// Claude-specific: assumes the "hooks" object-of-arrays shape and marks
    /// its own entries with `_overseer: true` for clean removal.
    JsonMerge,
    /// Merge/remove specific string `entries` into/from a named top-level
    /// JSON array field (HARNESSES.md — opencode's `instructions` array).
    /// Unlike `JsonMerge`'s hook objects, these are bare strings with no room
    /// for an `_overseer` sentinel, so uninstall removes exactly `entries`
    /// rather than anything tagged as Overseer's.
    JsonArrayMerge { key: &'static str, entries: Vec<String> },
}

pub trait AgentAdapter: Send + Sync {
    // --- install (install-time, pure) ---

    /// User config dir for this agent (e.g. `~/.claude`). Resolved at call time.
    fn user_config_dir(&self) -> Option<PathBuf>;

    /// Files to install at the user level. Pure — no I/O.
    fn install_files(&self) -> Vec<InstalledFile>;

    /// Paths (relative to `user_config_dir()`) from a previous install layout
    /// that `overseer install`/`--uninstall` should delete outright. Empty by
    /// default — only an adapter that has actually renamed/restructured its
    /// installed files needs to override this.
    fn legacy_paths(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    /// Whether `spawn_command`'s launch will actually succeed, not just start
    /// and immediately crash — checked with real I/O (`Path::exists`), so
    /// this is deliberately kept out of `install_files`'s otherwise-pure
    /// contract. Default `true`: most harnesses launch fine even without
    /// Overseer's install content, they just won't report status. Override
    /// when a missing file makes the launch command *itself* fail outright
    /// (verified live, HARNESSES.md: `pi --extension <missing path>` hard-
    /// errors and exits before the process ever starts responding) — without
    /// this check, a spawn against an uninstalled adapter registers a child
    /// that immediately crashes, sitting at `spawning` until the exit
    /// watcher's next sweep (up to 5s) catches it, which reads as "did this
    /// even work?" rather than a clear, immediate error.
    fn is_installed(&self) -> bool {
        true
    }

    /// Whether the user has actually run `overseer install` for this adapter
    /// — checked with real I/O (`Path::exists`), same as `is_installed`, but
    /// a different question: `is_installed` only answers "will `spawn_command`
    /// launch without crashing" (true for most adapters even with zero
    /// Overseer content on disk); this answers "did an install actually
    /// happen here." Used by the root-spawn picker (`n`) to decide which
    /// harnesses to offer.
    ///
    /// Default: every `install_files()` entry with `MergeStrategy::Overwrite`
    /// must exist under `user_config_dir()`. Those are the files install
    /// writes outright and only Overseer would ever create (a skill file, a
    /// plugin script, an extension) — unlike a `JsonMerge`/`JsonArrayMerge`
    /// target (e.g. claude's `settings.json`), which can pre-exist for
    /// reasons that have nothing to do with Overseer, so its mere presence
    /// isn't good evidence of an install. Works uniformly for all three
    /// adapters (each has at least one `Overwrite` file) with no override
    /// needed; vacuously `false` if an adapter somehow has none.
    fn overseer_installed(&self) -> bool {
        let Some(dir) = self.user_config_dir() else { return false };
        let artifacts: Vec<PathBuf> = self
            .install_files()
            .into_iter()
            .filter(|f| matches!(f.merge, MergeStrategy::Overwrite))
            .map(|f| f.path)
            .collect();
        !artifacts.is_empty() && artifacts.iter().all(|path| dir.join(path).exists())
    }

    // --- launch (runtime, pure) ---

    /// Returns the command to run inside the agent's PTY (program + args; no cwd/env).
    fn spawn_command(&self, ctx: &LaunchContext) -> Command;

    /// Returns env vars to inject into the agent's PTY.
    fn env_inject(&self, ctx: &LaunchContext) -> HashMap<String, String>;
}

/// Returns the adapter for the given name, or `None` if unknown.
pub fn adapter_for(name: &str) -> Option<Box<dyn AgentAdapter>> {
    match name {
        "claude" => Some(Box::new(claude::ClaudeAdapter::new())),
        "opencode" => Some(Box::new(opencode::OpencodeAdapter::new())),
        "pi" => Some(Box::new(pi::PiAdapter::new())),
        _ => None,
    }
}

/// Every name `adapter_for` recognizes, in a fixed order — what the
/// root-spawn picker (`n`) iterates over to find installed harnesses.
pub const ADAPTER_NAMES: [&str; 3] = ["claude", "opencode", "pi"];

/// Every `ADAPTER_NAMES` entry whose `overseer_installed()` is true, in
/// `ADAPTER_NAMES` order — the root-spawn picker's (`n`) adapter options,
/// before the literal "bare terminal" entry is appended.
pub fn installed_adapter_names() -> Vec<String> {
    let mut names = Vec::new();
    for name in ADAPTER_NAMES {
        if adapter_for(name).is_some_and(|a| a.overseer_installed()) {
            names.push(name.to_string());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn root_identity<'a>(agent_id: &'a AgentId, socket: &'a Path, repo: &'a str) -> AgentIdentity<'a> {
        AgentIdentity { agent_id, role: &AgentRole::Root, parent_id: None, socket, repo, depth: 1 }
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
        assert_eq!(env.get("OVERSEER_DEPTH").map(String::as_str), Some("1"));
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
            depth: 2,
        };
        let env = identity_env(&identity);
        assert_eq!(env.get("OVERSEER_ROLE").map(String::as_str), Some("child"));
        assert_eq!(env.get("OVERSEER_PARENT_ID"), Some(&parent.0.to_string()));
        assert_eq!(env.get("OVERSEER_DEPTH").map(String::as_str), Some("2"));
    }

    // ── installed_adapter_names ───────────────────────────────────────────────

    /// Points every adapter's config-dir env var at a fresh, empty temp
    /// dir at once — one `set_all` call, since a single-key `EnvGuard`
    /// can't be held three times over on one thread (see `test_env`'s doc
    /// comment).
    fn with_no_adapters_installed() -> crate::test_env::EnvGuard {
        let dir = std::env::temp_dir().join(format!("overseer-no-adapters-test-{}", uuid::Uuid::new_v4()));
        crate::test_env::EnvGuard::set_all(&[
            ("CLAUDE_CONFIG_DIR", dir.join("claude").to_str().unwrap()),
            ("XDG_CONFIG_HOME", dir.join("xdg").to_str().unwrap()),
            ("PI_CODING_AGENT_DIR", dir.join("pi").to_str().unwrap()),
        ])
    }

    #[test]
    fn installed_adapter_names_empty_when_nothing_installed() {
        let _env = with_no_adapters_installed();
        assert_eq!(installed_adapter_names(), Vec::<String>::new());
    }

    #[test]
    fn installed_adapter_names_finds_claude_alone_in_adapter_names_order() {
        let _env = with_no_adapters_installed();
        let claude_dir = std::env::var("CLAUDE_CONFIG_DIR").unwrap();
        let adapter = adapter_for("claude").unwrap();
        for file in adapter.install_files() {
            if matches!(file.merge, MergeStrategy::Overwrite) {
                let full = std::path::Path::new(&claude_dir).join(&file.path);
                std::fs::create_dir_all(full.parent().unwrap()).unwrap();
                std::fs::write(&full, "x").unwrap();
            }
        }
        assert_eq!(installed_adapter_names(), vec!["claude".to_string()]);
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
            task: String::new(),
            depth: 1,
        };
        let env = identity_env(&ctx.identity());
        assert_eq!(env.get("OVERSEER_AGENT_ID"), Some(&ctx.agent_id.0.to_string()));
        assert_eq!(env.get("OVERSEER_REPO").map(String::as_str), Some("myrepo"));
    }
}
