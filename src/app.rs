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

/// Active while awaiting y/n confirmation for `d`/`D`.
pub struct ConfirmState {
    pub agent_id: AgentId,
    pub recursive: bool,
}

pub struct App {
    pub ctx: Arc<AppCtx>,
    pub tick: u64,
    pub input: Option<InputState>,
    pub confirm: Option<ConfirmState>,
    pub status_message: Option<String>,
}

impl App {
    pub fn new(ctx: Arc<AppCtx>) -> Self {
        Self {
            ctx,
            tick: 0,
            input: None,
            confirm: None,
            status_message: None,
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
}
