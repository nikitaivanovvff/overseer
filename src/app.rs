use crate::agent::AgentTree;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Pane,
}

pub struct App {
    pub agent_tree: AgentTree,
    pub focus: Focus,
    pub tick: u64,
}

impl App {
    pub fn new() -> Self {
        Self {
            agent_tree: AgentTree::new(),
            focus: Focus::Tree,
            tick: 0,
        }
    }

    pub fn with_mock_data() -> Self {
        Self {
            agent_tree: AgentTree::with_mock_data(),
            focus: Focus::Tree,
            tick: 0,
        }
    }

    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }


}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}
