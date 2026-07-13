use thiserror::Error;

use crate::agent::{AgentId, AgentRegistry, AgentRole};
use crate::session::SessionManager;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DropError {
    #[error("unknown agent: {0}")]
    NotFound(AgentId),
    #[error("workspaces can only be dropped through the TUI")]
    RootRequiresTui,
    #[error("agent has children — use --recursive to drop the whole subtree")]
    HasChildrenNeedsRecursive,
}

/// Kills the agent's PTY session and deregisters it (and, if `recursive`, its whole
/// subtree). Sessions are killed children-before-parent so nothing is ever orphaned.
///
/// `allow_root` gates AGENTS.md's "root agents cannot be dropped via IPC — only via
/// the TUI" rule: the IPC `Drop` handler calls this with `false`, the TUI's drop
/// keybind calls it directly (same process, no socket round-trip) with `true`.
pub fn drop_agent(
    registry: &AgentRegistry,
    sessions: &SessionManager,
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
        sessions.kill(descendant_id);
    }

    registry.remove(id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::spawn::{spawn_agent, SpawnRequest};
    use crate::config::Config;
    use std::path::PathBuf;

    fn make_registry_and_sessions() -> (AgentRegistry, SessionManager) {
        (AgentRegistry::new(), SessionManager::dry_run())
    }

    fn spawn(
        registry: &AgentRegistry,
        sessions: &SessionManager,
        role: AgentRole,
        parent_id: Option<AgentId>,
    ) -> AgentId {
        spawn_agent(
            registry,
            sessions,
            &PathBuf::from("/tmp/overseer.sock"),
            &Config::default(),
            SpawnRequest {
                role,
                parent_id,
                task: "task".to_string(),
                name: None,
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
        let (registry, sessions) = make_registry_and_sessions();
        let err = drop_agent(&registry, &sessions, &AgentId::new(), false, false).unwrap_err();
        assert!(matches!(err, DropError::NotFound(_)));
    }

    #[test]
    fn drop_root_via_command_is_rejected() {
        let (registry, sessions) = make_registry_and_sessions();
        let root_id = spawn(&registry, &sessions, AgentRole::Root, None);
        let err = drop_agent(&registry, &sessions, &root_id, false, false).unwrap_err();
        assert_eq!(err, DropError::RootRequiresTui);
        assert_eq!(registry.snapshot().len(), 1); // untouched
    }

    #[test]
    fn drop_root_via_tui_succeeds() {
        let (registry, sessions) = make_registry_and_sessions();
        let root_id = spawn(&registry, &sessions, AgentRole::Root, None);
        drop_agent(&registry, &sessions, &root_id, false, true).unwrap();
        assert!(registry.snapshot().is_empty());
    }

    #[test]
    fn drop_leaf_child_succeeds_without_recursive() {
        let (registry, sessions) = make_registry_and_sessions();
        let root_id = spawn(&registry, &sessions, AgentRole::Root, None);
        let child_id = spawn(&registry, &sessions, AgentRole::Child, Some(root_id.clone()));
        drop_agent(&registry, &sessions, &child_id, false, false).unwrap();
        assert_eq!(registry.snapshot().len(), 1);
    }

    #[test]
    fn drop_non_recursive_with_children_is_rejected() {
        let (registry, sessions) = make_registry_and_sessions();
        let root_id = spawn(&registry, &sessions, AgentRole::Root, None);
        spawn(&registry, &sessions, AgentRole::Child, Some(root_id.clone()));
        let err = drop_agent(&registry, &sessions, &root_id, false, true).unwrap_err();
        assert_eq!(err, DropError::HasChildrenNeedsRecursive);
        assert_eq!(registry.snapshot().len(), 2); // untouched
    }

    #[test]
    fn drop_recursive_removes_whole_subtree() {
        let (registry, sessions) = make_registry_and_sessions();
        let root_id = spawn(&registry, &sessions, AgentRole::Root, None);
        let child_id = spawn(&registry, &sessions, AgentRole::Child, Some(root_id.clone()));
        let grandchild_id = spawn(&registry, &sessions, AgentRole::Child, Some(child_id.clone()));
        spawn(&registry, &sessions, AgentRole::Child, Some(root_id.clone()));

        let postorder = registry.with_tree(|tree| tree.subtree_ids_postorder(&root_id)).unwrap();
        assert!(
            postorder.iter().position(|id| id == &grandchild_id).unwrap()
                < postorder.iter().position(|id| id == &child_id).unwrap()
        );
        assert_eq!(postorder.last(), Some(&root_id));

        drop_agent(&registry, &sessions, &root_id, true, true).unwrap();
        assert!(registry.snapshot().is_empty());
    }
}
