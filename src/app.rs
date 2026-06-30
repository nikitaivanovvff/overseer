use std::sync::Arc;

use crate::agent::AgentRegistry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Pane,
}

pub struct App {
    pub registry: Arc<AgentRegistry>,
    pub focus: Focus,
    pub tick: u64,
}

impl App {
    pub fn new(registry: Arc<AgentRegistry>) -> Self {
        Self { registry, focus: Focus::Tree, tick: 0 }
    }

    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }
}
