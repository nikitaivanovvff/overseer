use std::sync::Arc;

use crate::agent::AgentId;
use crate::ipc::AppCtx;

/// What a text-input prompt (`n` / `s`) is being collected for.
#[derive(Debug, Clone)]
pub enum PendingAction {
    SpawnRoot,
    SpawnChild { parent_id: AgentId },
}

/// Active when the user is typing a task description for `n`/`s`.
pub struct InputState {
    pub action: PendingAction,
    pub buffer: String,
}

/// What a y/n confirmation prompt is asking about.
#[derive(Debug, Clone)]
pub enum ConfirmAction {
    Drop { agent_id: AgentId, recursive: bool },
    /// `q`/`Ctrl-C` with live agents: v1 has no persistence,
    /// quitting kills every running agent, so it's confirmed rather than
    /// silent — the one real regression from the tmux backend it replaces.
    Quit,
}

/// Active while awaiting y/n confirmation for `d`/`D`, or for quitting with
/// live agents.
pub struct ConfirmState {
    pub action: ConfirmAction,
}

/// Which half of the tree|pane split receives keyboard input.
/// `Ctrl-l` (or `Enter`/`o`) on a live agent moves `Tree -> Pane`; `Ctrl-h` is
/// the only key `Pane` intercepts, moving back to `Tree` — everything else
/// forwards to the agent's PTY untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Pane,
}

pub struct App {
    pub ctx: Arc<AppCtx>,
    pub tick: u64,
    pub input: Option<InputState>,
    pub confirm: Option<ConfirmState>,
    pub status_message: Option<String>,
    pub focus: Focus,
}

impl App {
    pub fn new(ctx: Arc<AppCtx>) -> Self {
        Self {
            ctx,
            tick: 0,
            input: None,
            confirm: None,
            status_message: None,
            focus: Focus::Tree,
        }
    }

    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentRegistry;
    use crate::git::GitClient;
    use crate::session::SessionManager;
    use std::path::PathBuf;

    fn test_app() -> App {
        let ctx = Arc::new(AppCtx {
            registry: Arc::new(AgentRegistry::new()),
            sessions: Arc::new(SessionManager::dry_run()),
            socket: PathBuf::from("/tmp/overseer-test.sock"),
            git: Arc::new(GitClient::new()),
            watch_sessions: false,
        });
        App::new(ctx)
    }

    #[test]
    fn new_app_starts_with_no_input_or_confirm_pending() {
        let app = test_app();
        assert!(app.input.is_none());
        assert!(app.confirm.is_none());
    }

    #[test]
    fn new_app_starts_focused_on_the_tree() {
        let app = test_app();
        assert_eq!(app.focus, Focus::Tree);
    }
}
