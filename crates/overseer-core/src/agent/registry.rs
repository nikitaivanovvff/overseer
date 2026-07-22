use std::path::PathBuf;
use std::sync::Mutex;

use thiserror::Error;
use tokio::sync::broadcast;

use crate::agent::{AgentId, AgentNode, AgentRole, AgentStatus, AgentTree, Attention, AttentionUpdate};
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
        model_name: Option<String>,
        attention: Option<Attention>,
        adapter: String,
        /// The node's current (merged) branch — same "definitive value, not
        /// a delta" posture as `adapter`.
        branch: String,
        /// The node's current (merged) repo — same "definitive value, not a
        /// delta" posture as `branch`. Only ever changes for a root/workspace
        /// node (see `set_status_update_with_model`); a child's `repo` is
        /// fixed at spawn time and simply echoed back unchanged.
        repo: String,
        /// The node's current (merged) display name — kept equal to `repo`
        /// for a root node (a workspace is always named after its repo), and
        /// otherwise just echoed back unchanged for a child, whose name is
        /// its given/task-derived label and is never touched here.
        name: String,
        /// See `AgentNode::session_alive`. Carried on every `StatusChanged`
        /// broadcast (not just ones from `mark_session_exited`) so a client
        /// applying this event as the definitive value (`app::apply_event`)
        /// never has a stale copy.
        session_alive: bool,
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
    /// Explicit branch override. Defaults to "main" for root; a child always
    /// starts empty (`—` in the TUI) regardless of this field — its real
    /// branch is self-reported by its own hook/plugin the moment it fires
    /// from inside the worktree it sets up (see `Request::Status.branch`),
    /// never guessed or synthesized here.
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
                    model_name: None,
                    attention: None,
                    session_alive: true,
                    children: Vec::new(),
                    expanded: true,
                    status_since: std::time::Instant::now(),
                    last_status_pushed_at: None,
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
                // Empty (`—` in the TUI), not a synthesized `overseer/<id>`
                // placeholder — a child's real branch is self-reported by
                // its own hook/plugin once it fires from inside the
                // worktree the child sets up itself (`Request::Status.branch`),
                // same "explicit empty, never faked" convention as a non-git
                // root (see `ipc::handlers::Request::Start`).
                let branch = args.branch.unwrap_or_default();
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
                    model_name: None,
                    attention: None,
                    session_alive: true,
                    children: Vec::new(),
                    expanded: true,
                    status_since: std::time::Instant::now(),
                    last_status_pushed_at: None,
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
    /// bare-shell root running (say) opencode ever stops looking like "shell" in
    /// the registry. Each adapter's own SessionStart-equivalent install hook
    /// passes this once, alongside its very first status push; a spawned
    /// child re-asserting its already-correct adapter here is harmless
    /// (same value, idempotent). Whatever ends up here is what `Request::Spawn`
    /// defaults an omitted `--adapter` to (its own children run the same
    /// harness unless told otherwise) — see `ipc::handlers`.
    ///
    /// A `Done`/`Error` agent's own automatic idle-downgrade (the `Stop`-hook
    /// `idle` push, or a `Notification` idle-nag) must not clobber it back to
    /// `Idle` — an agent that already reported completion shouldn't silently
    /// un-complete itself the moment its turn ends. This is *not* a general
    /// lock: `Running`/`Blocked` still apply unconditionally, so a human or
    /// root re-prompting a done/errored agent's pane still moves it forward
    /// (`UserPromptSubmit`'s `running` push and onward). Only the specific
    /// `Idle` push is suppressed, and only against `Done`/`Error`.
    ///
    /// This is deliberately *not* generalized into the `pushed_at` staleness
    /// guard below, even though both are "precedence" rules: the idle-nag
    /// almost always fires chronologically *after* the done/error push it
    /// must not clobber (e.g. a `Stop` hook firing moments after the turn
    /// that reported `done`), so it isn't a stale/out-of-order push at all —
    /// it's a later, genuinely-later push that must still lose because of
    /// *what* it is, not *when* it arrived. Staleness rejection wouldn't
    /// catch this case (see `STATUS-RACE.md`).
    ///
    /// `pushed_at` guards against a different failure mode: two *separate*
    /// hook fires (each its own short-lived OS process, its own socket
    /// connection — see `ipc::server`) racing on scheduling/connection setup
    /// so the one that fired *earlier* arrives at the daemon *later*. Without
    /// this, last-write-wins on arrival order would let a stale push (e.g. a
    /// `blocked` push queued behind a slow `Notification` hook) silently
    /// clobber a fresher status (e.g. `running`, from a `PostToolUse` hook
    /// that fired later but connected faster) — see `STATUS-RACE.md`. A push
    /// whose `pushed_at` is older than the last *accepted* push for this node
    /// is dropped in full (status, `context_pct`, `adapter`, broadcast — all
    /// of it, since a push's other fields are just as stale as its status).
    /// `None` on the node (nothing accepted yet, e.g. fresh off `register`)
    /// never rejects — there's nothing to compare against yet.
    pub fn set_status(
        &self,
        id: &AgentId,
        status: AgentStatus,
        message: Option<String>,
        context_pct: Option<u8>,
        adapter: Option<String>,
        pushed_at: std::time::SystemTime,
    ) -> Result<(), RegistryError> {
        self.set_status_update(id, status, message, context_pct, false, None, adapter, pushed_at)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_status_update(
        &self,
        id: &AgentId,
        status: AgentStatus,
        message: Option<String>,
        context_pct: Option<u8>,
        clear_context: bool,
        attention_update: Option<AttentionUpdate>,
        adapter: Option<String>,
        pushed_at: std::time::SystemTime,
    ) -> Result<(), RegistryError> {
        self.set_status_update_with_model(
            id,
            status,
            message,
            context_pct,
            clear_context,
            None,
            None,
            None,
            attention_update,
            adapter,
            pushed_at,
        )
    }

    /// `branch` follows the exact same "preserve last known value" posture as
    /// `model_name`: `None` (or all-whitespace) leaves the node's existing
    /// value untouched, a non-empty value replaces it. Self-reported by the
    /// pushing agent's own hook/plugin (`Request::Status.branch`) — the
    /// registry never guesses or synthesizes one.
    ///
    /// `repo` follows the same posture but is additionally gated on role:
    /// it only ever updates a **root** node's `repo` (and, kept in lockstep,
    /// `name` — a workspace is always named after its repo, same invariant
    /// `spawn_root` establishes at registration). A child's `repo`/`name` are
    /// never touched here even if a caller passes one — a child's worktree
    /// commonly lives in a sibling directory with its own git root, so
    /// self-reporting from cwd the way root does would rename it to its own
    /// worktree instead of the workspace it belongs to, and its `name` is a
    /// given/task-derived label that must never drift on its own. This is
    /// the same "live top-up" mechanism as `branch`: a workspace's name,
    /// static since `overseer start`, corrects itself the moment the first
    /// real status push (e.g. a harness's SessionStart-equivalent hook)
    /// fires from inside whatever directory the user `cd`'d the bare shell
    /// into — not on every `cd` itself, since Overseer has no push signal
    /// for a bare shell with nothing running inside it yet.
    #[allow(clippy::too_many_arguments)]
    pub fn set_status_update_with_model(
        &self,
        id: &AgentId,
        status: AgentStatus,
        message: Option<String>,
        context_pct: Option<u8>,
        clear_context: bool,
        model_name: Option<String>,
        branch: Option<String>,
        repo: Option<String>,
        attention_update: Option<AttentionUpdate>,
        adapter: Option<String>,
        pushed_at: std::time::SystemTime,
    ) -> Result<(), RegistryError> {
        let applied = {
            let mut guard = self.tree.lock().unwrap_or_else(|e| e.into_inner());
            match guard.find_mut(id) {
                Some(node) => {
                    let is_stale = node.last_status_pushed_at.is_some_and(|last| pushed_at < last);
                    if is_stale {
                        None
                    } else {
                        node.last_status_pushed_at = Some(pushed_at);

                        let suppress_idle_downgrade = status == AgentStatus::Idle
                            && matches!(node.status, AgentStatus::Done | AgentStatus::Error);
                        if !suppress_idle_downgrade {
                            // Compare *before* overwriting — a repeated same-status
                            // push (e.g. PostToolUse spam while `running`) must not
                            // reset the clock (ATTENTION.md).
                            if node.status != status {
                                node.status_since = std::time::Instant::now();
                            }
                            node.status = status.clone();
                        }
                        if let Some(pct) = context_pct {
                            node.context_pct = Some(pct);
                        } else if clear_context {
                            node.context_pct = None;
                        }
                        if let Some(model_name) = model_name.filter(|name| !name.trim().is_empty()) {
                            node.model_name = Some(model_name);
                        }
                        if let Some(branch) = branch.filter(|b| !b.trim().is_empty()) {
                            node.branch = branch;
                        }
                        if let Some(repo) = repo.filter(|r| !r.trim().is_empty()) {
                            if node.role == AgentRole::Root {
                                node.repo = repo.clone();
                                node.name = repo;
                            }
                        }
                        match attention_update {
                            Some(AttentionUpdate::Set { attention }) => node.attention = Some(attention),
                            Some(AttentionUpdate::Clear { kind }) => {
                                if node.attention.as_ref().is_some_and(|current| current.kind == kind) {
                                    node.attention = None;
                                }
                            }
                            None if status == AgentStatus::Running
                                && node.attention.as_ref().is_some_and(|a| a.kind.is_provider_limit()) =>
                            {
                                node.attention = None;
                            }
                            None => {}
                        }
                        if matches!(status, AgentStatus::Done | AgentStatus::Error) {
                            node.attention = None;
                        }
                        if let Some(adapter) = adapter {
                            node.adapter = adapter;
                        }
                        Some((
                            node.status.clone(),
                            node.context_pct,
                            node.model_name.clone(),
                            node.attention.clone(),
                            node.adapter.clone(),
                            node.branch.clone(),
                            node.repo.clone(),
                            node.name.clone(),
                            node.session_alive,
                        ))
                    }
                }
                None => return Err(RegistryError::UnknownAgent(id.clone())),
            }
        };
        let Some((
            new_status,
            new_context_pct,
            new_model_name,
            new_attention,
            new_adapter,
            new_branch,
            new_repo,
            new_name,
            session_alive,
        )) = applied
        else {
            return Ok(());
        };
        // Broadcast the node's actual resulting status, not necessarily the
        // pushed one — a suppressed idle-downgrade (above) must not tell
        // attach clients (which apply this as the definitive value, see
        // `app::apply_event`) that the status became `Idle` when it didn't.
        let _ = self.events.send(RegistryEvent::StatusChanged {
            agent_id: id.clone(),
            status: new_status,
            message,
            context_pct: new_context_pct,
            model_name: new_model_name,
            attention: new_attention,
            adapter: new_adapter,
            branch: new_branch,
            repo: new_repo,
            name: new_name,
            session_alive,
        });
        Ok(())
    }

    /// Records that `id`'s PTY has actually exited — the *only* writer of
    /// `AgentNode::session_alive`, called unconditionally by the daemon's
    /// exit-code sweep (`ipc::server::sweep_exited_sessions`) for every id
    /// `SessionManager::drain_exits()` yields, independent of whatever
    /// `status` already says. This is what lets a self-reported `done` agent
    /// (task complete, session still running — see `AgentNode::session_alive`
    /// doc comment) be told apart from one whose process has genuinely died,
    /// which `status` alone can't distinguish (both read `Done`).
    ///
    /// No-op (and no broadcast) for an unknown id or one already marked
    /// exited — `session_alive` only ever goes `true -> false`, once, so a
    /// repeated sweep tick over the same exited id (shouldn't happen since
    /// `drain_exits` only yields an id once, but cheap to guard) doesn't
    /// spam attach clients with redundant events.
    ///
    /// Always broadcasts a `StatusChanged` when it does flip the flag, even
    /// though `status` itself may be unchanged — an attach client needs this
    /// pushed immediately (not just at the next full `Snapshot`), since
    /// nothing else about this transition necessarily produces its own
    /// status push.
    pub fn mark_session_exited(&self, id: &AgentId) {
        let applied = {
            let mut guard = self.tree.lock().unwrap_or_else(|e| e.into_inner());
            match guard.find_mut(id) {
                Some(node) if node.session_alive => {
                    node.session_alive = false;
                    Some((
                        node.status.clone(),
                        node.context_pct,
                        node.model_name.clone(),
                        node.attention.clone(),
                        node.adapter.clone(),
                        node.branch.clone(),
                        node.repo.clone(),
                        node.name.clone(),
                    ))
                }
                _ => None,
            }
        };
        let Some((status, context_pct, model_name, attention, adapter, branch, repo, name)) = applied else {
            return;
        };
        let _ = self.events.send(RegistryEvent::StatusChanged {
            agent_id: id.clone(),
            status,
            message: None,
            context_pct,
            model_name,
            attention,
            adapter,
            branch,
            repo,
            name,
            session_alive: false,
        });
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

    /// Returns derived depth and direct-child count under one tree lock for
    /// the spawn handler's centralized admission checks.
    pub fn spawn_metrics(&self, id: &AgentId) -> Option<(usize, usize)> {
        let guard = self.tree.lock().unwrap_or_else(|e| e.into_inner());
        let node = guard.find(id)?;
        Some((guard.depth(id)?, node.children.len()))
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
    fn register_child_starts_with_an_empty_branch_not_a_synthesized_one() {
        // A child's real branch is self-reported later, via a `Request::Status`
        // push from its own hook/plugin once it sets up its worktree — the
        // registry must never fake one up front.
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
        assert_eq!(child_result.branch, "", "a freshly registered child must get an explicit empty branch, not a faked one");
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
            .set_status(&AgentId::new(), AgentStatus::Done, None, None, None, std::time::SystemTime::now())
            .unwrap_err();
        assert!(matches!(err, RegistryError::UnknownAgent(_)));
    }

    #[test]
    fn set_status_with_context_pct_updates_it() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        reg.set_status(&result.id, AgentStatus::Running, None, Some(42), None, std::time::SystemTime::now()).unwrap();
        assert_eq!(reg.get(&result.id).unwrap().context_pct, Some(42));
    }

    #[test]
    fn status_update_with_model_persists_across_lifecycle_only_pushes() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        reg.set_status_update_with_model(
            &result.id,
            AgentStatus::Running,
            None,
            None,
            false,
            Some("anthropic/claude-sonnet-5".to_string()),
            None,
            None,
            None,
            None,
            std::time::SystemTime::now(),
        )
        .unwrap();
        reg.set_status(&result.id, AgentStatus::Idle, None, None, None, std::time::SystemTime::now())
            .unwrap();
        assert_eq!(reg.get(&result.id).unwrap().model_name.as_deref(), Some("anthropic/claude-sonnet-5"));
    }

    // ── branch self-report (mirrors model_name above) ─────────────────────────

    #[test]
    fn set_status_update_with_model_updates_branch() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        reg.set_status_update_with_model(
            &result.id,
            AgentStatus::Running,
            None,
            None,
            false,
            None,
            Some("ovsr/auth-module".to_string()),
            None,
            None,
            None,
            std::time::SystemTime::now(),
        )
        .unwrap();
        assert_eq!(reg.get(&result.id).unwrap().branch, "ovsr/auth-module");
    }

    #[test]
    fn branch_persists_across_lifecycle_only_pushes() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        reg.set_status_update_with_model(
            &result.id,
            AgentStatus::Running,
            None,
            None,
            false,
            None,
            Some("ovsr/auth-module".to_string()),
            None,
            None,
            None,
            std::time::SystemTime::now(),
        )
        .unwrap();
        // A later push that doesn't carry a branch (e.g. a plain PostToolUse
        // hook that failed to detect one) must not erase the last known value.
        reg.set_status(&result.id, AgentStatus::Idle, None, None, None, std::time::SystemTime::now())
            .unwrap();
        assert_eq!(reg.get(&result.id).unwrap().branch, "ovsr/auth-module");
    }

    #[test]
    fn blank_branch_push_is_ignored() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        reg.set_status_update_with_model(
            &result.id,
            AgentStatus::Running,
            None,
            None,
            false,
            None,
            Some("ovsr/auth-module".to_string()),
            None,
            None,
            None,
            std::time::SystemTime::now(),
        )
        .unwrap();
        reg.set_status_update_with_model(
            &result.id,
            AgentStatus::Running,
            None,
            None,
            false,
            None,
            Some("   ".to_string()),
            None,
            None,
            None,
            std::time::SystemTime::now(),
        )
        .unwrap();
        assert_eq!(reg.get(&result.id).unwrap().branch, "ovsr/auth-module", "an all-whitespace branch must not overwrite a known one");
    }

    #[test]
    fn status_changed_broadcast_carries_the_current_branch() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        let mut rx = reg.subscribe();
        reg.set_status_update_with_model(
            &result.id,
            AgentStatus::Running,
            None,
            None,
            false,
            None,
            Some("ovsr/auth-module".to_string()),
            None,
            None,
            None,
            std::time::SystemTime::now(),
        )
        .unwrap();
        match rx.try_recv().unwrap() {
            RegistryEvent::StatusChanged { branch, .. } => assert_eq!(branch, "ovsr/auth-module"),
            other => panic!("expected StatusChanged, got {other:?}"),
        }
    }

    // ── repo/name self-report (mirrors branch above, root-only) ──────────────

    #[test]
    fn set_status_update_with_model_updates_repo_and_name_for_a_root() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("projects")).unwrap();
        reg.set_status_update_with_model(
            &result.id,
            AgentStatus::Running,
            None,
            None,
            false,
            None,
            None,
            Some("overseer".to_string()),
            None,
            None,
            std::time::SystemTime::now(),
        )
        .unwrap();
        let dto = reg.get(&result.id).unwrap();
        assert_eq!(dto.repo, "overseer");
        assert_eq!(dto.name, "overseer", "a workspace's tree label follows its repo");
    }

    #[test]
    fn repo_persists_across_lifecycle_only_pushes() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("projects")).unwrap();
        reg.set_status_update_with_model(
            &result.id,
            AgentStatus::Running,
            None,
            None,
            false,
            None,
            None,
            Some("overseer".to_string()),
            None,
            None,
            std::time::SystemTime::now(),
        )
        .unwrap();
        // A later push that doesn't self-report a repo (e.g. a plain
        // PostToolUse hook) must not revert the name back to the stale one.
        reg.set_status(&result.id, AgentStatus::Idle, None, None, None, std::time::SystemTime::now())
            .unwrap();
        let dto = reg.get(&result.id).unwrap();
        assert_eq!(dto.repo, "overseer");
        assert_eq!(dto.name, "overseer");
    }

    #[test]
    fn blank_repo_push_is_ignored() {
        let reg = AgentRegistry::new();
        // `make_register_root` hardcodes `repo: "overseer"` regardless of the
        // name given, matching the real `spawn_root` invariant that a root's
        // `name` and `repo` start equal.
        let result = reg.register(make_register_root("overseer")).unwrap();
        reg.set_status_update_with_model(
            &result.id,
            AgentStatus::Running,
            None,
            None,
            false,
            None,
            None,
            Some("   ".to_string()),
            None,
            None,
            std::time::SystemTime::now(),
        )
        .unwrap();
        let dto = reg.get(&result.id).unwrap();
        assert_eq!(dto.repo, "overseer", "an all-whitespace repo must not overwrite the registered one");
        assert_eq!(dto.name, "overseer");
    }

    #[test]
    fn repo_push_never_touches_a_childs_repo_or_name() {
        // A child's `repo` is fixed at spawn (inherited from its parent) and
        // its `name` is a given/task-derived label — self-reporting a repo
        // from a child's own cwd (commonly a sibling worktree with its own
        // git root) must never rename it to that worktree.
        let reg = AgentRegistry::new();
        let root = reg.register(make_register_root("overseer")).unwrap();
        let child = reg
            .register(RegisterArgs {
                id: None,
                name: "auth-module".to_string(),
                role: AgentRole::Child,
                parent_id: Some(root.id.clone()),
                adapter: "claude".to_string(),
                repo: "overseer".to_string(),
                cwd: PathBuf::from("."),
                branch: None,
                initial_status: AgentStatus::Running,
            })
            .unwrap();
        reg.set_status_update_with_model(
            &child.id,
            AgentStatus::Running,
            None,
            None,
            false,
            None,
            None,
            Some("overseer-auth-module".to_string()),
            None,
            None,
            std::time::SystemTime::now(),
        )
        .unwrap();
        let dto = reg.get(&child.id).unwrap();
        assert_eq!(dto.repo, "overseer", "a child's repo must stay the workspace it belongs to");
        assert_eq!(dto.name, "auth-module", "a child's name is its given label, never repo-derived");
    }

    #[test]
    fn status_changed_broadcast_carries_the_current_repo_and_name() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("projects")).unwrap();
        let mut rx = reg.subscribe();
        reg.set_status_update_with_model(
            &result.id,
            AgentStatus::Running,
            None,
            None,
            false,
            None,
            None,
            Some("overseer".to_string()),
            None,
            None,
            std::time::SystemTime::now(),
        )
        .unwrap();
        match rx.try_recv().unwrap() {
            RegistryEvent::StatusChanged { repo, name, .. } => {
                assert_eq!(repo, "overseer");
                assert_eq!(name, "overseer");
            }
            other => panic!("expected StatusChanged, got {other:?}"),
        }
    }

    #[test]
    fn set_status_without_context_pct_keeps_existing_value() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        reg.set_status(&result.id, AgentStatus::Running, None, Some(42), None, std::time::SystemTime::now()).unwrap();
        reg.set_status(&result.id, AgentStatus::Idle, None, None, None, std::time::SystemTime::now()).unwrap();
        assert_eq!(reg.get(&result.id).unwrap().context_pct, Some(42));
    }

    // ── Done/Error must survive an automatic idle-downgrade ──────────────────

    #[test]
    fn done_survives_a_later_idle_push() {
        // Mirrors `sweep_does_not_downgrade_an_already_done_agent_to_error`
        // in ipc::server, but for the hook-driven idle-push path (the
        // Stop-hook / idle-nag), not the PTY-exit sweep.
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        reg.set_status(&result.id, AgentStatus::Done, None, None, None, std::time::SystemTime::now()).unwrap();

        // The agent's own Stop hook fires moments later and pushes idle —
        // that must not clobber the done it already explicitly reported.
        reg.set_status(&result.id, AgentStatus::Idle, None, None, None, std::time::SystemTime::now()).unwrap();

        assert_eq!(
            reg.get(&result.id).unwrap().status,
            AgentStatus::Done,
            "an explicit done must survive a later automatic idle push"
        );
    }

    #[test]
    fn done_agent_still_moves_to_running_on_new_work() {
        // The escape hatch: Done/Error is only sticky against the automatic
        // idle-downgrade, not against genuine new work resuming.
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        reg.set_status(&result.id, AgentStatus::Done, None, None, None, std::time::SystemTime::now()).unwrap();

        reg.set_status(&result.id, AgentStatus::Running, None, None, None, std::time::SystemTime::now()).unwrap();

        assert_eq!(
            reg.get(&result.id).unwrap().status,
            AgentStatus::Running,
            "a re-prompted done agent must still be able to move to running"
        );
    }

    #[test]
    fn error_survives_a_later_idle_push() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        reg.set_status(&result.id, AgentStatus::Error, None, None, None, std::time::SystemTime::now()).unwrap();

        reg.set_status(&result.id, AgentStatus::Idle, None, None, None, std::time::SystemTime::now()).unwrap();

        assert_eq!(reg.get(&result.id).unwrap().status, AgentStatus::Error);
    }

    #[test]
    fn suppressed_idle_downgrade_still_updates_context_pct_and_adapter() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        reg.set_status(&result.id, AgentStatus::Done, None, None, None, std::time::SystemTime::now()).unwrap();

        reg.set_status(&result.id, AgentStatus::Idle, None, Some(77), Some("opencode".to_string()), std::time::SystemTime::now()).unwrap();

        let node = reg.get(&result.id).unwrap();
        assert_eq!(node.status, AgentStatus::Done, "status stays Done");
        assert_eq!(node.context_pct, Some(77), "context_pct still applies even when the status push is suppressed");
        assert_eq!(node.adapter, "opencode", "adapter still applies even when the status push is suppressed");
    }

    #[test]
    fn suppressed_idle_downgrade_broadcasts_the_actual_status_not_idle() {
        // Attach clients apply the broadcast status as the definitive value
        // (see app::apply_event) — broadcasting the suppressed `Idle` here
        // would reproduce the same clobber bug on the client side.
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        reg.set_status(&result.id, AgentStatus::Done, None, None, None, std::time::SystemTime::now()).unwrap();

        let mut rx = reg.subscribe();
        reg.set_status(&result.id, AgentStatus::Idle, None, None, None, std::time::SystemTime::now()).unwrap();

        match rx.try_recv().unwrap() {
            RegistryEvent::StatusChanged { status, .. } => {
                assert_eq!(status, AgentStatus::Done, "broadcast must reflect the node's real status, not the suppressed push");
            }
            other => panic!("expected StatusChanged, got {other:?}"),
        }
    }

    // ── status_since (ATTENTION.md) ───────────────────────────────────────────

    #[test]
    fn set_status_same_status_keeps_status_since() {
        // make_register_root starts a node as Running (see above).
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        let before = reg.with_tree(|t| t.find(&result.id).unwrap().status_since);

        reg.set_status(&result.id, AgentStatus::Running, None, None, None, std::time::SystemTime::now()).unwrap();

        let after = reg.with_tree(|t| t.find(&result.id).unwrap().status_since);
        assert_eq!(before, after, "a repeated same-status push (e.g. PostToolUse spam) must not reset the clock");
    }

    #[test]
    fn set_status_actual_change_resets_status_since() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        let before = reg.with_tree(|t| t.find(&result.id).unwrap().status_since);

        std::thread::sleep(std::time::Duration::from_millis(5));
        reg.set_status(&result.id, AgentStatus::Blocked, None, None, None, std::time::SystemTime::now()).unwrap();

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
        reg.set_status(&result.id, AgentStatus::Running, None, Some(10), None, std::time::SystemTime::now()).unwrap();

        let mut rx = reg.subscribe();
        reg.set_status(&result.id, AgentStatus::Blocked, Some("waiting".to_string()), None, None, std::time::SystemTime::now()).unwrap();
        match rx.try_recv().unwrap() {
            RegistryEvent::StatusChanged { agent_id, status, message, context_pct, .. } => {
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
        let err = reg.set_status(&AgentId::new(), AgentStatus::Done, None, None, None, std::time::SystemTime::now()).unwrap_err();
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

    // ── pushed_at staleness guard (STATUS-RACE.md) ────────────────────────────
    //
    // Each Claude Code hook fire is its own short-lived OS process making its
    // own fresh connection to the daemon, with no ordering between them. A
    // push that *fired* earlier can still *arrive* later than one that fired
    // after it (e.g. a `Notification`→`blocked` process slower to schedule
    // than a following `PostToolUse`→`running` one). These tests simulate
    // that by calling `set_status` with an out-of-order `pushed_at`, not by
    // racing real connections — the guard lives entirely in the registry, so
    // arrival order at this layer *is* `set_status` call order.

    #[test]
    fn a_push_with_an_older_timestamp_arriving_later_does_not_clobber_a_newer_status() {
        // The exact bug: `blocked` (fired first, e.g. a permission prompt)
        // arrives at the daemon *after* `running` (fired later, e.g. the
        // PostToolUse right after the user approved) because its hook
        // process was slower to schedule/connect. The `running` push must
        // win because it was pushed later, even though it arrived first.
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        let t0 = std::time::SystemTime::now();
        let earlier = t0 - std::time::Duration::from_millis(500);
        let later = t0;

        // `running` (pushed_at = later) arrives first.
        reg.set_status(&result.id, AgentStatus::Running, None, None, None, later).unwrap();
        // `blocked` (pushed_at = earlier) arrives second, but it's stale.
        reg.set_status(&result.id, AgentStatus::Blocked, None, None, None, earlier).unwrap();

        assert_eq!(
            reg.get(&result.id).unwrap().status,
            AgentStatus::Running,
            "a push with an older pushed_at must not override a push with a newer pushed_at, regardless of arrival order"
        );
    }

    #[test]
    fn a_stale_push_does_not_broadcast() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        let t0 = std::time::SystemTime::now();
        let earlier = t0 - std::time::Duration::from_millis(500);

        reg.set_status(&result.id, AgentStatus::Running, None, None, None, t0).unwrap();

        let mut rx = reg.subscribe();
        reg.set_status(&result.id, AgentStatus::Blocked, None, None, None, earlier).unwrap();

        assert!(rx.try_recv().is_err(), "a stale push must not broadcast anything");
    }

    #[test]
    fn a_stale_push_does_not_apply_context_pct_or_adapter_either() {
        // The whole push is stale, not just the status field -- an outdated
        // push's context_pct/adapter are just as untrustworthy as its status.
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        let t0 = std::time::SystemTime::now();
        let earlier = t0 - std::time::Duration::from_millis(500);

        reg.set_status(&result.id, AgentStatus::Running, None, Some(50), None, t0).unwrap();
        reg.set_status(&result.id, AgentStatus::Blocked, None, Some(90), Some("opencode".to_string()), earlier).unwrap();

        let node = reg.get(&result.id).unwrap();
        assert_eq!(node.status, AgentStatus::Running);
        assert_eq!(node.context_pct, Some(50), "stale push's context_pct must not apply");
        assert_eq!(node.adapter, "claude", "stale push's adapter must not apply");
    }

    #[test]
    fn a_push_with_a_newer_timestamp_still_applies_normally() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        let t0 = std::time::SystemTime::now();
        let later = t0 + std::time::Duration::from_millis(500);

        reg.set_status(&result.id, AgentStatus::Running, None, None, None, t0).unwrap();
        reg.set_status(&result.id, AgentStatus::Blocked, None, None, None, later).unwrap();

        assert_eq!(reg.get(&result.id).unwrap().status, AgentStatus::Blocked);
    }

    #[test]
    fn a_push_with_the_same_timestamp_still_applies() {
        // Not stale unless strictly older -- two pushes that happen to share
        // a timestamp (coarse clock resolution) must not deadlock each other
        // out.
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        let t0 = std::time::SystemTime::now();

        reg.set_status(&result.id, AgentStatus::Running, None, None, None, t0).unwrap();
        reg.set_status(&result.id, AgentStatus::Blocked, None, None, None, t0).unwrap();

        assert_eq!(reg.get(&result.id).unwrap().status, AgentStatus::Blocked);
    }

    #[test]
    fn first_push_ever_is_never_rejected_as_stale() {
        // A freshly registered node has no last_status_pushed_at yet -- there's
        // nothing to compare the first push against.
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        let ancient = std::time::SystemTime::UNIX_EPOCH;

        reg.set_status(&result.id, AgentStatus::Blocked, None, None, None, ancient).unwrap();

        assert_eq!(reg.get(&result.id).unwrap().status, AgentStatus::Blocked);
    }

    #[test]
    fn stale_idle_downgrade_attempt_is_rejected_by_timestamp_before_the_suppress_rule_even_applies() {
        // Belt-and-suspenders: even without the dedicated Done/Error-vs-Idle
        // suppress rule, an idle push that's also stale would be rejected on
        // timestamp grounds alone.
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        let t0 = std::time::SystemTime::now();
        let earlier = t0 - std::time::Duration::from_millis(500);

        reg.set_status(&result.id, AgentStatus::Done, None, None, None, t0).unwrap();
        reg.set_status(&result.id, AgentStatus::Idle, None, None, None, earlier).unwrap();

        assert_eq!(reg.get(&result.id).unwrap().status, AgentStatus::Done);
    }

    fn attention(kind: crate::agent::AttentionKind, observed_at: std::time::SystemTime) -> AttentionUpdate {
        AttentionUpdate::Set {
            attention: Attention { kind, message: None, retry_at: None, observed_at },
        }
    }

    #[test]
    fn unrelated_lifecycle_events_do_not_clear_permission_attention() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        let now = std::time::SystemTime::now();
        reg.set_status_update(
            &result.id,
            AgentStatus::Blocked,
            None,
            None,
            false,
            Some(attention(crate::agent::AttentionKind::Permission, now)),
            None,
            now,
        )
        .unwrap();
        reg.set_status(&result.id, AgentStatus::Idle, None, None, None, now).unwrap();
        assert_eq!(reg.get(&result.id).unwrap().attention.unwrap().kind, crate::agent::AttentionKind::Permission);
    }

    #[test]
    fn permission_reply_clears_only_permission_attention() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        let now = std::time::SystemTime::now();
        reg.set_status_update(
            &result.id,
            AgentStatus::Blocked,
            None,
            None,
            false,
            Some(attention(crate::agent::AttentionKind::Permission, now)),
            None,
            now,
        )
        .unwrap();
        reg.set_status_update(
            &result.id,
            AgentStatus::Running,
            None,
            None,
            false,
            Some(AttentionUpdate::Clear { kind: crate::agent::AttentionKind::Permission }),
            None,
            now,
        )
        .unwrap();
        assert!(reg.get(&result.id).unwrap().attention.is_none());
    }

    #[test]
    fn successful_activity_clears_provider_limit_attention() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        let now = std::time::SystemTime::now();
        reg.set_status_update(
            &result.id,
            AgentStatus::Running,
            None,
            None,
            false,
            Some(attention(crate::agent::AttentionKind::RateLimit, now)),
            None,
            now,
        )
        .unwrap();
        reg.set_status(&result.id, AgentStatus::Running, None, None, None, now).unwrap();
        assert!(reg.get(&result.id).unwrap().attention.is_none());
    }

    #[test]
    fn stale_attention_cannot_overwrite_a_newer_signal() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        let later = std::time::SystemTime::now();
        let earlier = later - std::time::Duration::from_secs(1);
        reg.set_status_update(
            &result.id,
            AgentStatus::Running,
            None,
            None,
            false,
            Some(attention(crate::agent::AttentionKind::Billing, later)),
            None,
            later,
        )
        .unwrap();
        reg.set_status_update(
            &result.id,
            AgentStatus::Blocked,
            None,
            None,
            false,
            Some(attention(crate::agent::AttentionKind::Permission, earlier)),
            None,
            earlier,
        )
        .unwrap();
        assert_eq!(reg.get(&result.id).unwrap().attention.unwrap().kind, crate::agent::AttentionKind::Billing);
    }

    #[test]
    fn terminal_status_clears_attention() {
        let reg = AgentRegistry::new();
        let result = reg.register(make_register_root("agent")).unwrap();
        let now = std::time::SystemTime::now();
        reg.set_status_update(
            &result.id,
            AgentStatus::Blocked,
            None,
            None,
            false,
            Some(attention(crate::agent::AttentionKind::Permission, now)),
            None,
            now,
        )
        .unwrap();
        reg.set_status(&result.id, AgentStatus::Done, None, None, None, now).unwrap();
        assert!(reg.get(&result.id).unwrap().attention.is_none());
    }
}
