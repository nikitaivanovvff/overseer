use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentId(pub Uuid);

impl AgentId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn short(&self) -> String {
        self.0.to_string()[..8].to_string()
    }
}

impl Default for AgentId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.short())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    Spawning,
    Running,
    Waiting,
    Done,
    Error,
}

impl AgentStatus {
    pub fn badge(&self) -> &'static str {
        match self {
            Self::Spawning => "…",
            Self::Running => "●",
            Self::Waiting => "○",
            Self::Done => "✓",
            Self::Error => "✗",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Spawning => "spawning",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Done => "done",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRole {
    Root,
    Child,
}

#[derive(Debug, Clone)]
pub struct AgentNode {
    pub id: AgentId,
    pub name: String,
    pub status: AgentStatus,
    pub role: AgentRole,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub children: Vec<AgentNode>,
    pub expanded: bool,
}

impl AgentNode {
    pub fn new_root(name: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            id: AgentId::new(),
            name: name.into(),
            status: AgentStatus::Running,
            role: AgentRole::Root,
            repo: Some(repo.into()),
            branch: Some("main".to_string()),
            children: Vec::new(),
            expanded: true,
        }
    }

    pub fn new_child(name: impl Into<String>, repo: impl Into<String>) -> Self {
        let id = AgentId::new();
        let branch = format!("overseer/{}", id.short());
        Self {
            id,
            name: name.into(),
            status: AgentStatus::Running,
            role: AgentRole::Child,
            repo: Some(repo.into()),
            branch: Some(branch),
            children: Vec::new(),
            expanded: true,
        }
    }
}

/// A flattened view of an AgentNode for rendering and navigation.
/// `prefix` contains the pre-computed tree connector string (e.g. "│ ├ ").
#[derive(Debug)]
pub struct FlatNode {
    pub id: AgentId,
    pub name: String,
    pub status: AgentStatus,
    pub role: AgentRole,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub depth: usize,
    pub has_children: bool,
    pub expanded: bool,
    pub is_last_sibling: bool,
    pub prefix: String,
}

#[derive(Debug, Default)]
pub struct AgentTree {
    pub roots: Vec<AgentNode>,
    pub cursor: usize,
}

impl AgentTree {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_root(&mut self, node: AgentNode) {
        self.roots.push(node);
    }

    /// Returns visible nodes in traversal order, respecting expand/collapse state.
    pub fn flatten(&self) -> Vec<FlatNode> {
        let mut result = Vec::new();
        let n = self.roots.len();
        for (i, root) in self.roots.iter().enumerate() {
            flatten_node(root, 0, i == n - 1, "", &mut result);
        }
        result
    }

    pub fn move_down(&mut self) {
        let count = self.flatten().len();
        if count > 0 {
            self.cursor = (self.cursor + 1).min(count - 1);
        }
    }

    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn toggle_expand(&mut self) {
        let flat = self.flatten();
        if let Some(node) = flat.get(self.cursor) {
            if node.has_children {
                let id = node.id.clone();
                toggle_expand_by_id(&mut self.roots, &id);
                let new_count = self.flatten().len();
                self.cursor = self.cursor.min(new_count.saturating_sub(1));
            }
        }
    }

    pub fn selected(&self) -> Option<FlatNode> {
        self.flatten().into_iter().nth(self.cursor)
    }

    pub fn with_mock_data() -> Self {
        let mut root1 = AgentNode::new_root("implement-auth", "overseer");

        let mut child_a = AgentNode::new_child("auth-module", "overseer");
        child_a.status = AgentStatus::Running;

        let mut child_b = AgentNode::new_child("write-tests", "overseer");
        child_b.status = AgentStatus::Done;

        let mut child_c = AgentNode::new_child("update-docs", "overseer");
        child_c.status = AgentStatus::Waiting;

        root1.children.push(child_a);
        root1.children.push(child_b);
        root1.children.push(child_c);

        let mut root2 = AgentNode::new_root("refactor-api", "overseer");
        root2.status = AgentStatus::Waiting;

        let mut tree = Self::new();
        tree.add_root(root1);
        tree.add_root(root2);
        tree
    }
}

// Recursively flatten, building the tree-connector prefix as we go.
// `indent` is the accumulated indentation string from ancestor levels.
fn flatten_node(
    node: &AgentNode,
    depth: usize,
    is_last: bool,
    indent: &str,
    result: &mut Vec<FlatNode>,
) {
    let connector = if depth == 0 {
        ""
    } else if is_last {
        "└ "
    } else {
        "├ "
    };

    result.push(FlatNode {
        id: node.id.clone(),
        name: node.name.clone(),
        status: node.status.clone(),
        role: node.role.clone(),
        repo: node.repo.clone(),
        branch: node.branch.clone(),
        depth,
        has_children: !node.children.is_empty(),
        expanded: node.expanded,
        is_last_sibling: is_last,
        prefix: format!("{indent}{connector}"),
    });

    if node.expanded {
        let child_indent = if depth == 0 {
            indent.to_string()
        } else if is_last {
            format!("{indent}  ")
        } else {
            format!("{indent}│ ")
        };
        let n = node.children.len();
        for (i, child) in node.children.iter().enumerate() {
            flatten_node(child, depth + 1, i == n - 1, &child_indent, result);
        }
    }
}

fn toggle_expand_by_id(nodes: &mut Vec<AgentNode>, target: &AgentId) -> bool {
    for node in nodes.iter_mut() {
        if &node.id == target {
            node.expanded = !node.expanded;
            return true;
        }
        if toggle_expand_by_id(&mut node.children, target) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tree() -> AgentTree {
        let mut root = AgentNode::new_root("root", "repo");
        let mut child_a = AgentNode::new_child("child-a", "repo");
        let grandchild = AgentNode::new_child("grandchild", "repo");
        child_a.children.push(grandchild);
        let child_b = AgentNode::new_child("child-b", "repo");
        root.children.push(child_a);
        root.children.push(child_b);
        let mut tree = AgentTree::new();
        tree.add_root(root);
        tree
    }

    #[test]
    fn test_flatten_empty() {
        let tree = AgentTree::new();
        assert!(tree.flatten().is_empty());
    }

    #[test]
    fn test_flatten_single_root_no_prefix() {
        let mut tree = AgentTree::new();
        tree.add_root(AgentNode::new_root("root", "repo"));
        let flat = tree.flatten();
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].prefix, "");
        assert_eq!(flat[0].depth, 0);
    }

    #[test]
    fn test_flatten_node_count() {
        let tree = make_tree();
        // root, child-a, grandchild, child-b
        assert_eq!(tree.flatten().len(), 4);
    }

    #[test]
    fn test_flatten_order() {
        let tree = make_tree();
        let flat = tree.flatten();
        assert_eq!(flat[0].name, "root");
        assert_eq!(flat[1].name, "child-a");
        assert_eq!(flat[2].name, "grandchild");
        assert_eq!(flat[3].name, "child-b");
    }

    #[test]
    fn test_flatten_depths() {
        let tree = make_tree();
        let flat = tree.flatten();
        assert_eq!(flat[0].depth, 0);
        assert_eq!(flat[1].depth, 1);
        assert_eq!(flat[2].depth, 2);
        assert_eq!(flat[3].depth, 1);
    }

    #[test]
    fn test_flatten_prefixes() {
        let tree = make_tree();
        let flat = tree.flatten();
        assert_eq!(flat[0].prefix, "");       // root
        assert_eq!(flat[1].prefix, "├ ");     // child-a (not last)
        assert_eq!(flat[2].prefix, "│ └ ");   // grandchild (parent not last, grandchild is last)
        assert_eq!(flat[3].prefix, "└ ");     // child-b (last)
    }

    #[test]
    fn test_flatten_last_sibling_flags() {
        let tree = make_tree();
        let flat = tree.flatten();
        assert!(flat[0].is_last_sibling);
        assert!(!flat[1].is_last_sibling);
        assert!(flat[2].is_last_sibling);
        assert!(flat[3].is_last_sibling);
    }

    #[test]
    fn test_flatten_collapsed_hides_children() {
        let mut tree = make_tree();
        let child_a_id = tree.flatten()[1].id.clone();
        toggle_expand_by_id(&mut tree.roots, &child_a_id);
        let flat = tree.flatten();
        assert_eq!(flat.len(), 3);
        assert_eq!(flat[0].name, "root");
        assert_eq!(flat[1].name, "child-a");
        assert_eq!(flat[2].name, "child-b");
    }

    #[test]
    fn test_flatten_multiple_roots() {
        let mut tree = AgentTree::new();
        tree.add_root(AgentNode::new_root("root1", "repo"));
        tree.add_root(AgentNode::new_root("root2", "repo"));
        let flat = tree.flatten();
        assert_eq!(flat.len(), 2);
        assert!(!flat[0].is_last_sibling);
        assert!(flat[1].is_last_sibling);
    }

    #[test]
    fn test_move_down_advances_cursor() {
        let mut tree = make_tree();
        tree.move_down();
        assert_eq!(tree.cursor, 1);
        tree.move_down();
        assert_eq!(tree.cursor, 2);
    }

    #[test]
    fn test_move_up_decrements_cursor() {
        let mut tree = make_tree();
        tree.cursor = 2;
        tree.move_up();
        assert_eq!(tree.cursor, 1);
        tree.move_up();
        assert_eq!(tree.cursor, 0);
    }

    #[test]
    fn test_move_up_at_top_stays() {
        let mut tree = make_tree();
        tree.move_up();
        assert_eq!(tree.cursor, 0);
    }

    #[test]
    fn test_move_down_at_bottom_stays() {
        let mut tree = make_tree();
        let last = tree.flatten().len() - 1;
        tree.cursor = last;
        tree.move_down();
        assert_eq!(tree.cursor, last);
    }

    #[test]
    fn test_move_down_on_empty_is_noop() {
        let mut tree = AgentTree::new();
        tree.move_down();
        assert_eq!(tree.cursor, 0);
    }

    #[test]
    fn test_toggle_expand_collapses_children() {
        let mut tree = make_tree();
        tree.cursor = 1;
        let before = tree.flatten().len();
        tree.toggle_expand();
        assert_eq!(tree.flatten().len(), before - 1);
    }

    #[test]
    fn test_toggle_expand_re_expands() {
        let mut tree = make_tree();
        tree.cursor = 1;
        tree.toggle_expand();
        tree.toggle_expand();
        assert_eq!(tree.flatten().len(), 4);
    }

    #[test]
    fn test_toggle_expand_on_leaf_is_noop() {
        let mut tree = make_tree();
        tree.cursor = 3;
        let before = tree.flatten().len();
        tree.toggle_expand();
        assert_eq!(tree.flatten().len(), before);
    }

    #[test]
    fn test_cursor_clamped_after_collapse() {
        let mut tree = make_tree();
        tree.cursor = 2;
        let child_a_id = tree.flatten()[1].id.clone();
        toggle_expand_by_id(&mut tree.roots, &child_a_id);
        let new_count = tree.flatten().len();
        tree.cursor = tree.cursor.min(new_count.saturating_sub(1));
        assert!(tree.cursor < tree.flatten().len());
    }

    #[test]
    fn test_selected_returns_cursor_node() {
        let tree = make_tree();
        assert_eq!(tree.selected().unwrap().name, "root");
    }

    #[test]
    fn test_selected_after_move() {
        let mut tree = make_tree();
        tree.move_down();
        assert_eq!(tree.selected().unwrap().name, "child-a");
    }

    #[test]
    fn test_selected_on_empty_is_none() {
        let tree = AgentTree::new();
        assert!(tree.selected().is_none());
    }

    #[test]
    fn test_has_children_flag() {
        let tree = make_tree();
        let flat = tree.flatten();
        assert!(flat[0].has_children);
        assert!(flat[1].has_children);
        assert!(!flat[2].has_children);
        assert!(!flat[3].has_children);
    }
}
