use std::sync::Arc;

use crate::agent::AgentId;
use crate::ipc::AppCtx;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Pane,
}

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
    pub focus: Focus,
    pub tick: u64,
    pub input: Option<InputState>,
    pub confirm: Option<ConfirmState>,
    pub status_message: Option<String>,
}

impl App {
    pub fn new(ctx: Arc<AppCtx>) -> Self {
        Self {
            ctx,
            focus: Focus::Tree,
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
