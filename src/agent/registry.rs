use std::path::PathBuf;
use std::sync::Mutex;

use thiserror::Error;
use tokio::sync::broadcast;

use crate::agent::{AgentId, AgentNode, AgentRole, AgentStatus, AgentTree};
use crate::ipc::protocol::AgentDto;

/// Capacity of the registry's broadcast channel — generous enough that a slow
/// attach client only misses events under sustained, unrealistic load; a
/// lagged receiver just skips ahead rather than blocking a writer (AGENTS.md
/// "status is push, not pull" — pushes must never back up on the sender).
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Broadcast to every attached client on every mutation (DAEMON.md "Attach
/// protocol"). Cheap to construct (no runtime needed until a receiver
/// `.recv().await`s), so `AgentRegistry` can build one unconditionally —
/// existing sync callers with zero subscribers just get an ignored `Err` back
/// from `send`.
#[derive(Debug, Clone)]
pub enum RegistryEvent {
    Registered { agent: AgentDto },
    Removed { agent_id: AgentId },
    StatusChanged {
        agent_id: AgentId,
        status: AgentStatus,
        message: Option<String>,
        context_pct: Option<u8>,
    },
    /// The daemon itself is exiting (`overseer shutdown`) — distinct from any
    /// per-agent event. Broadcast explicitly via `announce_shutdown`, not a
    /// side effect of any registry mutation.
    Shutdown,
}

pub struct AgentRegistry {
    tree: Mutex<AgentTree>,
    events: broadcast::Sender<RegistryEvent>,
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
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self { tree: Mutex::new(AgentTree::new()), events }
    }

    pub fn from_tree(tree: AgentTree) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self { tree: Mutex::new(tree), events }
    }

    /// Subscribes to every registration/removal/status-change from this point
    /// on — the feed an attach connection forwards to its client. A missed
    /// event (receiver too slow) surfaces as `RecvError::Lagged`, not silent
    /// data loss; callers should treat that as "re-sync via a fresh snapshot".
    pub fn subscribe(&self) -> broadcast::Receiver<RegistryEvent> {
        self.events.subscribe()
    }

    /// Broadcasts that the daemon is exiting (`overseer shutdown`), so every
    /// attached client's `Backend::Daemon` sees it independently of this
    /// request's own response — delivery happens on each attach connection's
    /// own forwarding task, not tied to when *this* caller's response gets
    /// written.
    pub fn announce_shutdown(&self) {
        let _ = self.events.send(RegistryEvent::Shutdown);
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
                    status_since: std::time::Instant::now(),
                };
                let dto = AgentDto::from_node(&node, None);
                guard.add_root(node);
                drop(guard);
                let _ = self.events.send(RegistryEvent::Registered { agent: dto });
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
                    status_since: std::time::Instant::now(),
                };
                let dto = AgentDto::from_node(&node, Some(parent_id.clone()));
                if guard.insert_child(&parent_id, node) {
                    drop(guard);
                    let _ = self.events.send(RegistryEvent::Registered { agent: dto });
                    Ok(RegisterResult { id, branch })
                } else {
                    Err(RegistryError::UnknownParent(parent_id))
                }
            }
        }
    }

    /// Updates the status of the agent with the given id. `context_pct` of
    /// `None` leaves the node's existing value untouched — most status pushes
    /// don't carry one. `message` isn't stored on the node (no field for it),
    /// but is forwarded verbatim on the broadcast event for attach clients.
    ///
    /// `adapter` lets a session self-identify its actual harness — a root's
    /// adapter is always registered as the honest-but-uninformative "shell"
    /// (`overseer start` never launches one), so this is the only way a
    /// bare-shell root running (say) pi ever stops looking like "shell" in
    /// the registry. Each adapter's own SessionStart-equivalent install hook
    /// passes this once, alongside its very first status push; a spawned
    /// child re-asserting its already-correct adapter here is harmless
    /// (same value, idempotent). Whatever ends up here is what `Request::Spawn`
    /// defaults an omitted `--adapter` to (its own children run the same
    /// harness unless told otherwise) — see `ipc::handlers`.
    pub fn set_status(
        &self,
        id: &AgentId,
        status: AgentStatus,
        message: Option<String>,
        context_pct: Option<u8>,
        adapter: Option<String>,
    ) -> Result<(), RegistryError> {
        let new_context_pct = {
            let mut guard = self.tree.lock().unwrap_or_else(|e| e.into_inner());
            match guard.find_mut(id) {
                Some(node) => {
                    // Compare *before* overwriting — a repeated same-status
                    // push (e.g. PostToolUse spam while `running`) must not
                    // reset the clock (ATTENTION.md).
                    if node.status != status {
                        node.status_since = std::time::Instant::now();
                    }
                    node.status = status.clone();
                    if let Some(pct) = context_pct {
                        node.context_pct = Some(pct);
                    }
                    if let Some(adapter) = adapter {
                        node.adapter = adapter;
                    }
                    node.context_pct
                }
                None => return Err(RegistryError::UnknownAgent(id.clone())),
            }
        };
        let _ = self.events.send(RegistryEvent::StatusChanged {
            agent_id: id.clone(),
            status,
            message,
            context_pct: new_context_pct,
        });
        Ok(())
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
        let removed = {
            let mut guard = self.tree.lock().unwrap_or_else(|e| e.into_inner());
            guard.remove(id)
        };
        if removed {
            let _ = self.events.send(RegistryEvent::Removed { agent_id: id.clone() });
        }
        removed
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
            .set_status(&AgentId::new(), AgentStatus::Done, None, None, None)
            .unwrap_err();
        assert!(matches!(err, RegistryError::UnknownAgent(_)));
    }

    #[test]
    fn set_status_with_context_pct_updates_it() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        reg.set_status(&result.id, AgentStatus::Running, None, Some(42), None).unwrap();
        assert_eq!(reg.get(&result.id).unwrap().context_pct, Some(42));
    }

    #[test]
    fn set_status_without_context_pct_keeps_existing_value() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        reg.set_status(&result.id, AgentStatus::Running, None, Some(42), None).unwrap();
        reg.set_status(&result.id, AgentStatus::Idle, None, None, None).unwrap();
        assert_eq!(reg.get(&result.id).unwrap().context_pct, Some(42));
    }

    // ── status_since (ATTENTION.md) ───────────────────────────────────────────

    #[test]
    fn set_status_same_status_keeps_status_since() {
        // make_register_root starts a node as Running (see above).
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        let before = reg.with_tree(|t| t.find(&result.id).unwrap().status_since);

        reg.set_status(&result.id, AgentStatus::Running, None, None, None).unwrap();

        let after = reg.with_tree(|t| t.find(&result.id).unwrap().status_since);
        assert_eq!(before, after, "a repeated same-status push (e.g. PostToolUse spam) must not reset the clock");
    }

    #[test]
    fn set_status_actual_change_resets_status_since() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        let before = reg.with_tree(|t| t.find(&result.id).unwrap().status_since);

        std::thread::sleep(std::time::Duration::from_millis(5));
        reg.set_status(&result.id, AgentStatus::Blocked, None, None, None).unwrap();

        let after = reg.with_tree(|t| t.find(&result.id).unwrap().status_since);
        assert!(after > before, "an actual status change must reset the clock");
    }

    #[test]
    fn register_seeds_status_since_freshly() {
        let reg = AgentRegistry::new();
        let before = std::time::Instant::now();
        let result = reg.register(make_register_root("agent")).unwrap();
        let status_since = reg.with_tree(|t| t.find(&result.id).unwrap().status_since);
        assert!(status_since >= before, "a freshly registered node's clock must start now, not earlier");
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

    // ── broadcast events ──────────────────────────────────────────────────────

    #[test]
    fn register_root_broadcasts_registered_with_no_parent() {
        let reg = AgentRegistry::new();
        let mut rx = reg.subscribe();
        let result = reg.register(make_register_root("root-agent")).unwrap();
        match rx.try_recv().unwrap() {
            RegistryEvent::Registered { agent } => {
                assert_eq!(agent.id, result.id);
                assert!(agent.parent_id.is_none());
            }
            other => panic!("expected Registered, got {other:?}"),
        }
    }

    #[test]
    fn register_child_broadcasts_registered_with_parent_id() {
        let reg = AgentRegistry::new();
        let root = reg.register(make_register_root("root")).unwrap();
        let mut rx = reg.subscribe();
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
        match rx.try_recv().unwrap() {
            RegistryEvent::Registered { agent } => {
                assert_eq!(agent.id, child.id);
                assert_eq!(agent.parent_id, Some(root.id));
            }
            other => panic!("expected Registered, got {other:?}"),
        }
    }

    #[test]
    fn failed_register_does_not_broadcast() {
        let reg = AgentRegistry::new();
        let mut rx = reg.subscribe();
        let err = reg
            .register(RegisterArgs {
                id: None,
                name: "orphan".to_string(),
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
        assert!(rx.try_recv().is_err(), "a rejected register must not broadcast anything");
    }

    #[test]
    fn set_status_broadcasts_status_changed_with_message_and_merged_context_pct() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        reg.set_status(&result.id, AgentStatus::Running, None, Some(10), None).unwrap();

        let mut rx = reg.subscribe();
        reg.set_status(&result.id, AgentStatus::Blocked, Some("waiting".to_string()), None, None).unwrap();
        match rx.try_recv().unwrap() {
            RegistryEvent::StatusChanged { agent_id, status, message, context_pct } => {
                assert_eq!(agent_id, result.id);
                assert_eq!(status, AgentStatus::Blocked);
                assert_eq!(message.as_deref(), Some("waiting"));
                // Broadcast carries the node's current (merged) value, not the
                // absent value from this particular push.
                assert_eq!(context_pct, Some(10));
            }
            other => panic!("expected StatusChanged, got {other:?}"),
        }
    }

    #[test]
    fn set_status_unknown_agent_does_not_broadcast() {
        let reg = AgentRegistry::new();
        let mut rx = reg.subscribe();
        let err = reg.set_status(&AgentId::new(), AgentStatus::Done, None, None, None).unwrap_err();
        assert!(matches!(err, RegistryError::UnknownAgent(_)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn remove_broadcasts_removed() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        let mut rx = reg.subscribe();
        assert!(reg.remove(&result.id));
        match rx.try_recv().unwrap() {
            RegistryEvent::Removed { agent_id } => assert_eq!(agent_id, result.id),
            other => panic!("expected Removed, got {other:?}"),
        }
    }

    #[test]
    fn remove_unknown_agent_does_not_broadcast() {
        let reg = AgentRegistry::new();
        let mut rx = reg.subscribe();
        assert!(!reg.remove(&AgentId::new()));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn multiple_subscribers_each_receive_the_same_event() {
        let reg = AgentRegistry::new();
        let mut rx1 = reg.subscribe();
        let mut rx2 = reg.subscribe();
        let result = reg.register(make_register_root("agent")).unwrap();
        assert!(matches!(rx1.try_recv().unwrap(), RegistryEvent::Registered { agent } if agent.id == result.id));
        assert!(matches!(rx2.try_recv().unwrap(), RegistryEvent::Registered { agent } if agent.id == result.id));
    }

    #[test]
    fn announce_shutdown_broadcasts_shutdown_event() {
        let reg = AgentRegistry::new();
        let mut rx = reg.subscribe();
        reg.announce_shutdown();
        assert!(matches!(rx.try_recv().unwrap(), RegistryEvent::Shutdown));
    }
}
