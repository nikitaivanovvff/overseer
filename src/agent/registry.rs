use std::path::PathBuf;
use std::sync::Mutex;

use thiserror::Error;

use crate::agent::{AgentId, AgentNode, AgentRole, AgentStatus, AgentTree};

pub struct AgentRegistry {
    tree: Mutex<AgentTree>,
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("unknown agent: {0}")]
    UnknownAgent(AgentId),
    #[error("unknown parent: {0}")]
    UnknownParent(AgentId),
    #[error("child role requires --parent-id")]
    MissingParent,
}

pub struct RegisterArgs {
    pub id: Option<AgentId>,
    pub name: String,
    pub role: AgentRole,
    pub parent_id: Option<AgentId>,
    pub adapter: String,
    pub repo: String,
    pub cwd: PathBuf,
    /// Explicit branch override. Defaults to "main" for root, "overseer/<id>" for child.
    pub branch: Option<String>,
    /// Status the node starts in. Explicit at every call site — e.g. a bare-shell
    /// root starts `Idle` (nothing running yet), an adapter-launched child starts
    /// `Running` (it auto-launches immediately).
    pub initial_status: AgentStatus,
}

#[derive(Debug)]
pub struct RegisterResult {
    pub id: AgentId,
    pub branch: String,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self { tree: Mutex::new(AgentTree::new()) }
    }

    pub fn from_tree(tree: AgentTree) -> Self {
        Self { tree: Mutex::new(tree) }
    }

    pub fn register(&self, args: RegisterArgs) -> Result<RegisterResult, RegistryError> {
        let mut guard = self.tree.lock().unwrap_or_else(|e| e.into_inner());
        match args.role {
            AgentRole::Root => {
                let id = args.id.unwrap_or_default();
                let branch = args.branch.unwrap_or_else(|| "main".to_string());
                let node = AgentNode {
                    id: id.clone(),
                    name: args.name,
                    status: args.initial_status,
                    role: AgentRole::Root,
                    repo: args.repo,
                    branch: branch.clone(),
                    adapter: args.adapter,
                    cwd: args.cwd,
                    context_pct: None,
                    children: Vec::new(),
                    expanded: true,
                };
                guard.add_root(node);
                Ok(RegisterResult { id, branch })
            }
            AgentRole::Child => {
                let parent_id = args.parent_id.ok_or(RegistryError::MissingParent)?;

                let id = args.id.unwrap_or_default();
                let branch = format!("overseer/{}", id.short());
                let node = AgentNode {
                    id: id.clone(),
                    name: args.name,
                    status: args.initial_status,
                    role: AgentRole::Child,
                    repo: args.repo,
                    branch: branch.clone(),
                    adapter: args.adapter,
                    cwd: args.cwd,
                    context_pct: None,
                    children: Vec::new(),
                    expanded: true,
                };
                if guard.insert_child(&parent_id, node) {
                    Ok(RegisterResult { id, branch })
                } else {
                    Err(RegistryError::UnknownParent(parent_id))
                }
            }
        }
    }

    /// Updates the status of the agent with the given id. `message` is accepted
    /// per the protocol but not stored. `context_pct` of `None` leaves the
    /// node's existing value untouched — most status pushes don't carry one.
    pub fn set_status(
        &self,
        id: &AgentId,
        status: AgentStatus,
        _message: Option<String>,
        context_pct: Option<u8>,
    ) -> Result<(), RegistryError> {
        let mut guard = self.tree.lock().unwrap_or_else(|e| e.into_inner());
        match guard.find_mut(id) {
            Some(node) => {
                node.status = status;
                if let Some(pct) = context_pct {
                    node.context_pct = Some(pct);
                }
                Ok(())
            }
            None => Err(RegistryError::UnknownAgent(id.clone())),
        }
    }

    /// Returns a flattened snapshot of all agents as wire DTOs, for `list`.
    pub fn snapshot(&self) -> Vec<crate::ipc::protocol::AgentDto> {
        let guard = self.tree.lock().unwrap_or_else(|e| e.into_inner());
        tree_to_dtos(&guard.roots)
    }

    /// Returns the DTO for a single agent by id.
    pub fn get(&self, id: &AgentId) -> Option<crate::ipc::protocol::AgentDto> {
        let guard = self.tree.lock().unwrap_or_else(|e| e.into_inner());
        find_dto(&guard.roots, id, None)
    }

    pub fn with_tree<R>(&self, f: impl FnOnce(&AgentTree) -> R) -> R {
        let guard = self.tree.lock().unwrap_or_else(|e| e.into_inner());
        f(&guard)
    }

    /// Removes an agent from the tree. Returns `true` if found and removed.
    pub fn remove(&self, id: &AgentId) -> bool {
        let mut guard = self.tree.lock().unwrap_or_else(|e| e.into_inner());
        guard.remove(id)
    }

    pub fn with_tree_mut<R>(&self, f: impl FnOnce(&mut AgentTree) -> R) -> R {
        let mut guard = self.tree.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn tree_to_dtos(roots: &[AgentNode]) -> Vec<crate::ipc::protocol::AgentDto> {
    let mut result = Vec::new();
    for root in roots {
        collect_dtos(root, None, &mut result);
    }
    result
}

fn collect_dtos(
    node: &AgentNode,
    parent_id: Option<AgentId>,
    result: &mut Vec<crate::ipc::protocol::AgentDto>,
) {
    result.push(crate::ipc::protocol::AgentDto::from_node(node, parent_id));
    for child in &node.children {
        collect_dtos(child, Some(node.id.clone()), result);
    }
}

fn find_dto(
    nodes: &[AgentNode],
    target: &AgentId,
    parent_id: Option<&AgentId>,
) -> Option<crate::ipc::protocol::AgentDto> {
    for node in nodes {
        if node.id == *target {
            return Some(crate::ipc::protocol::AgentDto::from_node(node, parent_id.cloned()));
        }
        if let Some(found) = find_dto(&node.children, target, Some(&node.id)) {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_register_root(name: &str) -> RegisterArgs {
        RegisterArgs {
            id: None,
            name: name.to_string(),
            role: AgentRole::Root,
            parent_id: None,
            adapter: "claude".to_string(),
            repo: "overseer".to_string(),
            cwd: PathBuf::from("."),
            branch: None,
            initial_status: AgentStatus::Running,
        }
    }

    #[test]
    fn register_root_returns_id_and_main_branch() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("root-agent")).unwrap();
        assert_eq!(result.branch, "main");
    }

    #[test]
    fn register_child_returns_id_and_overseer_branch() {
        let reg = AgentRegistry::new();
        let root = reg.register(make_register_root("root")).unwrap();
        let child_result = reg
            .register(RegisterArgs {
                id: None,
                name: "child".to_string(),
                role: AgentRole::Child,
                parent_id: Some(root.id.clone()),
                adapter: "claude".to_string(),
                repo: "overseer".to_string(),
                cwd: PathBuf::from("."),
                branch: None,
                initial_status: AgentStatus::Running,
            })
            .unwrap();
        assert!(child_result.branch.starts_with("overseer/"));
    }

    #[test]
    fn register_child_unknown_parent_returns_error() {
        let reg = AgentRegistry::new();
        let err = reg
            .register(RegisterArgs {
                id: None,
                name: "child".to_string(),
                role: AgentRole::Child,
                parent_id: Some(AgentId::new()),
                adapter: "claude".to_string(),
                repo: "overseer".to_string(),
                cwd: PathBuf::from("."),
                branch: None,
                initial_status: AgentStatus::Running,
            })
            .unwrap_err();
        assert!(matches!(err, RegistryError::UnknownParent(_)));
    }

    #[test]
    fn set_status_unknown_id_returns_error() {
        let reg = AgentRegistry::new();
        let err = reg
            .set_status(&AgentId::new(), AgentStatus::Done, None, None)
            .unwrap_err();
        assert!(matches!(err, RegistryError::UnknownAgent(_)));
    }

    #[test]
    fn set_status_with_context_pct_updates_it() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        reg.set_status(&result.id, AgentStatus::Running, None, Some(42)).unwrap();
        assert_eq!(reg.get(&result.id).unwrap().context_pct, Some(42));
    }

    #[test]
    fn set_status_without_context_pct_keeps_existing_value() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        reg.set_status(&result.id, AgentStatus::Running, None, Some(42)).unwrap();
        reg.set_status(&result.id, AgentStatus::Idle, None, None).unwrap();
        assert_eq!(reg.get(&result.id).unwrap().context_pct, Some(42));
    }

    #[test]
    fn snapshot_reflects_inserts_and_parent_id() {
        let reg = AgentRegistry::new();
        let root = reg.register(make_register_root("root")).unwrap();
        let child = reg
            .register(RegisterArgs {
                id: None,
                name: "child".to_string(),
                role: AgentRole::Child,
                parent_id: Some(root.id.clone()),
                adapter: "claude".to_string(),
                repo: "overseer".to_string(),
                cwd: PathBuf::from("."),
                branch: None,
                initial_status: AgentStatus::Running,
            })
            .unwrap();

        let dtos = reg.snapshot();
        assert_eq!(dtos.len(), 2);

        let root_dto = dtos.iter().find(|d| d.id == root.id).unwrap();
        assert!(root_dto.parent_id.is_none());

        let child_dto = dtos.iter().find(|d| d.id == child.id).unwrap();
        assert_eq!(child_dto.parent_id.as_ref(), Some(&root.id));
    }

    #[test]
    fn get_returns_dto_for_existing_agent() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("my-agent")).unwrap();
        let dto = reg.get(&result.id).unwrap();
        assert_eq!(dto.name, "my-agent");
        assert_eq!(dto.branch, "main");
    }

    #[test]
    fn get_returns_none_for_unknown_agent() {
        let reg = AgentRegistry::new();
        assert!(reg.get(&AgentId::new()).is_none());
    }

    #[test]
    fn register_root_with_initial_status_idle_is_reflected() {
        let reg = AgentRegistry::new();
        let mut args = make_register_root("shell-root");
        args.initial_status = AgentStatus::Idle;
        let result = reg.register(args).unwrap();
        let dto = reg.get(&result.id).unwrap();
        assert_eq!(dto.status, AgentStatus::Idle);
    }

    #[test]
    fn register_root_with_branch_override() {
        let reg = AgentRegistry::new();
        let result = reg
            .register(RegisterArgs {
                id: None,
                name: "my-task".to_string(),
                role: AgentRole::Root,
                parent_id: None,
                adapter: "claude".to_string(),
                repo: "myrepo".to_string(),
                cwd: PathBuf::from("."),
                branch: Some("feature/auth".to_string()),
                initial_status: AgentStatus::Running,
            })
            .unwrap();
        assert_eq!(result.branch, "feature/auth");
    }
}
