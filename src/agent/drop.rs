use thiserror::Error;

use crate::agent::spawn::tmux_session_name;
use crate::agent::{AgentId, AgentRegistry, AgentRole};
use crate::session::TmuxClient;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DropError {
    #[error("unknown agent: {0}")]
    NotFound(AgentId),
    #[error("root agents cannot be dropped via command — use the TUI")]
    RootRequiresTui,
    #[error("agent has children — use --recursive to drop the whole subtree")]
    HasChildrenNeedsRecursive,
}

/// Kills the agent's tmux session and deregisters it (and, if `recursive`, its whole
/// subtree). Sessions are killed children-before-parent so nothing is ever orphaned.
///
/// `allow_root` gates AGENTS.md's "root agents cannot be dropped via IPC — only via
/// the TUI" rule: the IPC `Drop` handler calls this with `false`, the TUI's drop
/// keybind calls it directly (same process, no socket round-trip) with `true`.
pub fn drop_agent(
    registry: &AgentRegistry,
    tmux: &TmuxClient,
    id: &AgentId,
    recursive: bool,
    allow_root: bool,
) -> Result<(), DropError> {
    let agent = registry.get(id).ok_or_else(|| DropError::NotFound(id.clone()))?;

    if agent.role == AgentRole::Root && !allow_root {
        return Err(DropError::RootRequiresTui);
    }

    let subtree = registry
        .with_tree(|tree| tree.subtree_ids_postorder(id))
        .ok_or_else(|| DropError::NotFound(id.clone()))?;

    if subtree.len() > 1 && !recursive {
        return Err(DropError::HasChildrenNeedsRecursive);
    }

    // Best-effort: a session may already be gone (e.g. reaped by the background
    // session watcher), which is not an error condition for a drop.
    for descendant_id in &subtree {
        let _ = tmux.kill_session(&tmux_session_name(descendant_id));
    }

    registry.remove(id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::spawn::{spawn_agent, SpawnRequest};
    use std::path::PathBuf;

    fn make_registry_and_tmux() -> (AgentRegistry, TmuxClient) {
        (AgentRegistry::new(), TmuxClient::dry_run())
    }

    fn spawn(
        registry: &AgentRegistry,
        tmux: &TmuxClient,
        role: AgentRole,
        parent_id: Option<AgentId>,
    ) -> AgentId {
        spawn_agent(
            registry,
            tmux,
            &PathBuf::from("/tmp/overseer.sock"),
            SpawnRequest {
                role,
                parent_id,
                task: "task".to_string(),
                adapter_name: "claude".to_string(),
                cwd: PathBuf::from("/tmp"),
                repo: "overseer".to_string(),
                branch: None,
            },
        )
        .unwrap()
        .id
    }

    #[test]
    fn drop_unknown_agent_errors() {
        let (registry, tmux) = make_registry_and_tmux();
        let err = drop_agent(&registry, &tmux, &AgentId::new(), false, false).unwrap_err();
        assert!(matches!(err, DropError::NotFound(_)));
    }

    #[test]
    fn drop_root_via_command_is_rejected() {
        let (registry, tmux) = make_registry_and_tmux();
        let root_id = spawn(&registry, &tmux, AgentRole::Root, None);
        let err = drop_agent(&registry, &tmux, &root_id, false, false).unwrap_err();
        assert_eq!(err, DropError::RootRequiresTui);
        assert_eq!(registry.snapshot().len(), 1); // untouched
    }

    #[test]
    fn drop_root_via_tui_succeeds() {
        let (registry, tmux) = make_registry_and_tmux();
        let root_id = spawn(&registry, &tmux, AgentRole::Root, None);
        drop_agent(&registry, &tmux, &root_id, false, true).unwrap();
        assert!(registry.snapshot().is_empty());
    }

    #[test]
    fn drop_leaf_child_succeeds_without_recursive() {
        let (registry, tmux) = make_registry_and_tmux();
        let root_id = spawn(&registry, &tmux, AgentRole::Root, None);
        let child_id = spawn(&registry, &tmux, AgentRole::Child, Some(root_id.clone()));
        drop_agent(&registry, &tmux, &child_id, false, false).unwrap();
        assert_eq!(registry.snapshot().len(), 1);
    }

    #[test]
    fn drop_non_recursive_with_children_is_rejected() {
        let (registry, tmux) = make_registry_and_tmux();
        let root_id = spawn(&registry, &tmux, AgentRole::Root, None);
        spawn(&registry, &tmux, AgentRole::Child, Some(root_id.clone()));
        let err = drop_agent(&registry, &tmux, &root_id, false, true).unwrap_err();
        assert_eq!(err, DropError::HasChildrenNeedsRecursive);
        assert_eq!(registry.snapshot().len(), 2); // untouched
    }

    #[test]
    fn drop_recursive_removes_whole_subtree() {
        let (registry, tmux) = make_registry_and_tmux();
        let root_id = spawn(&registry, &tmux, AgentRole::Root, None);
        spawn(&registry, &tmux, AgentRole::Child, Some(root_id.clone()));
        spawn(&registry, &tmux, AgentRole::Child, Some(root_id.clone()));
        drop_agent(&registry, &tmux, &root_id, true, true).unwrap();
        assert!(registry.snapshot().is_empty());
    }
}
