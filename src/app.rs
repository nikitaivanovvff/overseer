use crate::agent::AgentTree;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Pane,
}

pub struct App {
    pub agent_tree: AgentTree,
    pub focus: Focus,
}

impl App {
    pub fn new() -> Self {
        Self {
            agent_tree: AgentTree::new(),
            focus: Focus::Tree,
        }
    }

    pub fn with_mock_data() -> Self {
        Self {
            agent_tree: AgentTree::with_mock_data(),
            focus: Focus::Tree,
        }
    }

    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Tree => Focus::Pane,
            Focus::Pane => Focus::Tree,
        };
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}
