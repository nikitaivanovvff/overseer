use std::path::PathBuf;

use super::{AgentId, AgentNode, AgentRole, AgentStatus};

/// Mock/test-only constructor for a root node. Real registration always goes
/// through `AgentRegistry::register` — this exists purely for `with_mock_data`
/// and unit tests that need a node without going through the registry.
fn mock_root(name: impl Into<String>, repo: impl Into<String>) -> AgentNode {
    AgentNode {
        id: AgentId::new(),
        name: name.into(),
        status: AgentStatus::Running,
        role: AgentRole::Root,
        repo: repo.into(),
        branch: "main".to_string(),
        adapter: "claude".to_string(),
        cwd: PathBuf::from("."),
        context_pct: None,
        children: Vec::new(),
        expanded: true,
    }
}

/// Mock/test-only constructor for a child node. See `mock_root`.
fn mock_child(name: impl Into<String>, repo: impl Into<String>) -> AgentNode {
    let id = AgentId::new();
    let branch = format!("overseer/{}", id.short());
    AgentNode {
        id,
        name: name.into(),
        status: AgentStatus::Running,
        role: AgentRole::Child,
        repo: repo.into(),
        branch,
        adapter: "claude".to_string(),
        cwd: PathBuf::from("."),
        context_pct: None,
        children: Vec::new(),
        expanded: true,
    }
}

/// A flattened snapshot of one AgentNode for rendering and navigation.
/// `prefix` is the pre-computed tree-connector string (e.g. "│ ├ ") — it
/// already encodes depth/last-sibling for the renderer, so those aren't
/// carried as separate fields.
#[derive(Debug, Clone)]
pub struct FlatNode {
    pub id: AgentId,
    pub name: String,
    pub status: AgentStatus,
    pub role: AgentRole,
    pub repo: String,
    pub branch: String,
    pub context_pct: Option<u8>,
    pub has_children: bool,
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

    /// Returns (running, blocked, total) counts across ALL agents, ignoring
    /// expand/collapse state.
    pub fn agent_counts(&self) -> (usize, usize, usize) {
        fn count(nodes: &[AgentNode], running: &mut usize, blocked: &mut usize, total: &mut usize) {
            for node in nodes {
                *total += 1;
                match node.status {
                    AgentStatus::Running => *running += 1,
                    AgentStatus::Blocked => *blocked += 1,
                    _ => {}
                }
                count(&node.children, running, blocked, total);
            }
        }
        let (mut running, mut blocked, mut total) = (0, 0, 0);
        count(&self.roots, &mut running, &mut blocked, &mut total);
        (running, blocked, total)
    }

    pub fn move_down(&mut self) {
        let count = self.flatten().len();
        if count > 0 {
            // Clamp first so a stale cursor doesn't jump backward.
            let capped = self.cursor.min(count - 1);
            self.cursor = (capped + 1).min(count - 1);
        }
    }

    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Returns a mutable reference to the node with the given id (recursive).
    pub fn find_mut(&mut self, id: &AgentId) -> Option<&mut AgentNode> {
        find_node_mut(&mut self.roots, id)
    }

    /// Returns an immutable reference to the node with the given id (recursive).
    pub fn find(&self, id: &AgentId) -> Option<&AgentNode> {
        find_node(&self.roots, id)
    }

    /// Inserts `node` as a child of the node identified by `parent_id`.
    /// Returns `true` if the parent was found, `false` otherwise.
    pub fn insert_child(&mut self, parent_id: &AgentId, node: AgentNode) -> bool {
        if let Some(parent) = self.find_mut(parent_id) {
            parent.children.push(node);
            true
        } else {
            false
        }
    }

    /// Removes the node with the given id (root or descendant).
    /// Returns `true` if found and removed.
    pub fn remove(&mut self, id: &AgentId) -> bool {
        let removed = if let Some(pos) = self.roots.iter().position(|n| n.id == *id) {
            self.roots.remove(pos);
            true
        } else {
            remove_descendant(&mut self.roots, id)
        };
        if removed {
            // Removing a descendant can shrink the flattened list too (e.g. its own
            // children go with it), so the cursor must be clamped either way —
            // otherwise `selected()` can silently start returning `None`.
            let len = self.flatten().len();
            self.cursor = if len == 0 { 0 } else { self.cursor.min(len - 1) };
        }
        removed
    }

    /// Returns the ids of the subtree rooted at `id`, in post-order (children before
    /// the node itself). `None` if `id` is not found anywhere in the tree.
    pub fn subtree_ids_postorder(&self, id: &AgentId) -> Option<Vec<AgentId>> {
        let node = find_node(&self.roots, id)?;
        let mut out = Vec::new();
        collect_postorder(node, &mut out);
        Some(out)
    }

    pub fn toggle_expand(&mut self) {
        let flat = self.flatten();
        if let Some(node) = flat.get(self.cursor) {
            if node.has_children {
                let id = node.id.clone();
                if let Some(node) = self.find_mut(&id) {
                    node.expanded = !node.expanded;
                }
                let new_count = self.flatten().len();
                self.cursor = self.cursor.min(new_count.saturating_sub(1));
            }
        }
    }

    pub fn selected(&self) -> Option<FlatNode> {
        self.flatten().into_iter().nth(self.cursor)
    }

    pub fn with_mock_data() -> Self {
        let mut root1 = mock_root("implement-auth", "overseer");
        root1.context_pct = Some(45);

        let mut child_a = mock_child("auth-module", "overseer");
        child_a.context_pct = Some(23);

        let mut child_b = mock_child("write-tests", "overseer");
        child_b.status = AgentStatus::Done;
        child_b.context_pct = Some(87);

        let mut child_c = mock_child("update-docs", "overseer");
        child_c.status = AgentStatus::Blocked;
        child_c.context_pct = Some(5);

        root1.children.push(child_a);
        root1.children.push(child_b);
        root1.children.push(child_c);

        let mut root2 = mock_root("refactor-api", "overseer");
        root2.status = AgentStatus::Blocked;

        let mut root3 = mock_root("fix-login-bug", "overseer");
        root3.status = AgentStatus::Error;
        root3.context_pct = Some(62);

        let mut tree = Self::new();
        tree.add_root(root1);
        tree.add_root(root2);
        tree.add_root(root3);
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
        context_pct: node.context_pct,
        has_children: !node.children.is_empty(),
        prefix: format!("{indent}{connector}"),
    });

    if node.expanded {
        // When a root node (depth=0) is not the last root, its children need
        // a "│ " continuation bar so the visual tree shows root2 still follows.
        let child_indent = if is_last {
            if depth == 0 {
                String::new()
            } else {
                format!("{indent}  ")
            }
        } else {
            format!("{indent}│ ")
        };
        let n = node.children.len();
        for (i, child) in node.children.iter().enumerate() {
            flatten_node(child, depth + 1, i == n - 1, &child_indent, result);
        }
    }
}

/// Recursive mutable node lookup using split_first_mut to satisfy the borrow checker.
fn find_node_mut<'a>(nodes: &'a mut [AgentNode], id: &AgentId) -> Option<&'a mut AgentNode> {
    let (head, tail) = nodes.split_first_mut()?;
    if head.id == *id {
        return Some(head);
    }
    if let Some(found) = find_node_mut(&mut head.children, id) {
        return Some(found);
    }
    find_node_mut(tail, id)
}

/// Recursive immutable node lookup (root or descendant).
fn find_node<'a>(nodes: &'a [AgentNode], id: &AgentId) -> Option<&'a AgentNode> {
    for node in nodes {
        if node.id == *id {
            return Some(node);
        }
        if let Some(found) = find_node(&node.children, id) {
            return Some(found);
        }
    }
    None
}

/// Appends `node`'s subtree ids in post-order (children before the node itself).
fn collect_postorder(node: &AgentNode, out: &mut Vec<AgentId>) {
    for child in &node.children {
        collect_postorder(child, out);
    }
    out.push(node.id.clone());
}

fn remove_descendant(nodes: &mut [AgentNode], id: &AgentId) -> bool {
    for node in nodes.iter_mut() {
        if let Some(pos) = node.children.iter().position(|c| c.id == *id) {
            node.children.remove(pos);
            return true;
        }
        if remove_descendant(&mut node.children, id) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    // make_tree builds a 3-level tree to exercise the flatten/navigation logic at
    // depth > 1. The two-level constraint is enforced in the Phase 4 `spawn` handler,
    // not at the model level, so this fixture is valid for testing the traversal.
    fn make_tree() -> AgentTree {
        let mut root = mock_root("root", "repo");
        let mut child_a = mock_child("child-a", "repo");
        let grandchild = mock_child("grandchild", "repo");
        child_a.children.push(grandchild);
        let child_b = mock_child("child-b", "repo");
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
        tree.add_root(mock_root("root", "repo"));
        let flat = tree.flatten();
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].prefix, "");
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
    fn test_flatten_prefixes() {
        let tree = make_tree();
        let flat = tree.flatten();
        assert_eq!(flat[0].prefix, "");       // root
        assert_eq!(flat[1].prefix, "├ ");     // child-a (not last)
        assert_eq!(flat[2].prefix, "│ └ ");   // grandchild (parent not last, grandchild is last)
        assert_eq!(flat[3].prefix, "└ ");     // child-b (last)
    }

    #[test]
    fn test_flatten_multi_root_child_indent() {
        // Regression for child_indent depth==0 bug: children of a non-last root
        // must receive "│ " child_indent so their subtrees show the continuation bar.
        let mut tree = AgentTree::new();
        let mut root1 = mock_root("root1", "repo");
        let child = mock_child("child", "repo");
        let mut child_with_kids = mock_child("child-with-kids", "repo");
        // Give child-with-kids its own child so we can check grand-indent.
        let grandchild = mock_child("grandchild", "repo");
        child_with_kids.children.push(grandchild);
        root1.children.push(child);
        root1.children.push(child_with_kids);
        let root2 = mock_root("root2", "repo");
        tree.add_root(root1);
        tree.add_root(root2);

        let flat = tree.flatten();
        // root1 is not last: children should use "│ " indent
        assert_eq!(flat[1].prefix, "│ ├ "); // child (not last, under non-last root)
        assert_eq!(flat[2].prefix, "│ └ "); // child-with-kids (last, under non-last root)
        assert_eq!(flat[3].prefix, "│   └ "); // grandchild (last, parent is last under non-last root)
    }

    #[test]
    fn test_flatten_collapsed_hides_children() {
        let mut tree = make_tree();
        let child_a_id = tree.flatten()[1].id.clone();
        tree.find_mut(&child_a_id).unwrap().expanded = false;
        let flat = tree.flatten();
        assert_eq!(flat.len(), 3);
        assert_eq!(flat[0].name, "root");
        assert_eq!(flat[1].name, "child-a");
        assert_eq!(flat[2].name, "child-b");
    }

    #[test]
    fn test_flatten_multiple_roots() {
        let mut tree = AgentTree::new();
        tree.add_root(mock_root("root1", "repo"));
        tree.add_root(mock_root("root2", "repo"));
        let flat = tree.flatten();
        assert_eq!(flat.len(), 2);
        assert_eq!(flat[0].name, "root1");
        assert_eq!(flat[1].name, "root2");
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
    fn test_move_down_clamps_stale_cursor() {
        // cursor above count must not jump backward when move_down is called
        let mut tree = make_tree();
        tree.cursor = 99; // stale / out-of-bounds
        tree.move_down();
        let last = tree.flatten().len() - 1;
        assert_eq!(tree.cursor, last);
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
        tree.find_mut(&child_a_id).unwrap().expanded = false;
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

    #[test]
    fn test_find_mut_root() {
        let mut tree = make_tree();
        let root_id = tree.roots[0].id.clone();
        assert!(tree.find_mut(&root_id).is_some());
    }

    #[test]
    fn test_find_mut_child() {
        let mut tree = make_tree();
        let child_id = tree.roots[0].children[0].id.clone();
        let found = tree.find_mut(&child_id);
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "child-a");
    }

    #[test]
    fn test_find_mut_grandchild() {
        let mut tree = make_tree();
        let gc_id = tree.roots[0].children[0].children[0].id.clone();
        let found = tree.find_mut(&gc_id);
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "grandchild");
    }

    #[test]
    fn test_find_mut_unknown_returns_none() {
        let mut tree = make_tree();
        let unknown = AgentId::new();
        assert!(tree.find_mut(&unknown).is_none());
    }

    #[test]
    fn test_find_returns_immutable_ref_to_grandchild() {
        let tree = make_tree();
        let gc_id = tree.roots[0].children[0].children[0].id.clone();
        assert_eq!(tree.find(&gc_id).unwrap().name, "grandchild");
    }

    #[test]
    fn test_find_unknown_returns_none() {
        let tree = make_tree();
        assert!(tree.find(&AgentId::new()).is_none());
    }

    #[test]
    fn test_insert_child_under_existing_parent() {
        let mut tree = AgentTree::new();
        let root = mock_root("root", "repo");
        let root_id = root.id.clone();
        tree.add_root(root);

        let child = mock_child("child", "repo");
        let inserted = tree.insert_child(&root_id, child);

        assert!(inserted);
        assert_eq!(tree.roots[0].children.len(), 1);
        assert_eq!(tree.roots[0].children[0].name, "child");
    }

    #[test]
    fn test_insert_child_unknown_parent_returns_false() {
        let mut tree = AgentTree::new();
        tree.add_root(mock_root("root", "repo"));
        let unknown = AgentId::new();
        let child = mock_child("orphan", "repo");
        assert!(!tree.insert_child(&unknown, child));
    }

    #[test]
    fn remove_root_shrinks_tree() {
        let mut tree = AgentTree::new();
        let r1 = mock_root("r1", "repo");
        let r2 = mock_root("r2", "repo");
        let id1 = r1.id.clone();
        tree.add_root(r1);
        tree.add_root(r2);
        assert!(tree.remove(&id1));
        assert_eq!(tree.flatten().len(), 1);
        assert_eq!(tree.flatten()[0].name, "r2");
    }

    #[test]
    fn remove_child_shrinks_parent() {
        let mut tree = make_tree();
        let child_id = tree.roots[0].children[0].id.clone();
        assert!(tree.remove(&child_id));
        // grandchild was under child-a, so it's gone too
        assert!(tree.roots[0].children.iter().all(|c| c.name != "child-a"));
    }

    #[test]
    fn remove_unknown_returns_false() {
        let mut tree = make_tree();
        assert!(!tree.remove(&AgentId::new()));
    }

    #[test]
    fn subtree_ids_postorder_leaf_has_only_itself() {
        let tree = make_tree();
        let child_b_id = tree.roots[0].children[1].id.clone();
        assert_eq!(tree.subtree_ids_postorder(&child_b_id).unwrap(), vec![child_b_id]);
    }

    #[test]
    fn subtree_ids_postorder_nested_children_come_before_parent() {
        let tree = make_tree();
        let root_id = tree.roots[0].id.clone();
        let child_a_id = tree.roots[0].children[0].id.clone();
        let grandchild_id = tree.roots[0].children[0].children[0].id.clone();
        let child_b_id = tree.roots[0].children[1].id.clone();

        let ids = tree.subtree_ids_postorder(&root_id).unwrap();
        assert_eq!(ids, vec![grandchild_id, child_a_id, child_b_id, root_id]);
    }

    #[test]
    fn subtree_ids_postorder_unknown_id_returns_none() {
        let tree = make_tree();
        assert!(tree.subtree_ids_postorder(&AgentId::new()).is_none());
    }

    #[test]
    fn remove_clamps_cursor() {
        let mut tree = AgentTree::new();
        let r1 = mock_root("r1", "repo");
        let id1 = r1.id.clone();
        tree.add_root(r1);
        tree.cursor = 0;
        tree.remove(&id1);
        assert_eq!(tree.cursor, 0); // must not panic on empty
    }

    #[test]
    fn remove_descendant_also_clamps_cursor() {
        // Regression test: only the root-removal branch used to clamp the cursor,
        // so removing a child while the cursor pointed at (or past) it left
        // `selected()` returning `None` even though a valid node remained.
        let mut tree = make_tree();
        let child_a_id = tree.roots[0].children[0].id.clone();
        let grandchild_id = tree.roots[0].children[0].children[0].id.clone();

        // flatten order: root, child-a, grandchild, child-b — cursor on grandchild.
        tree.cursor = 2;
        assert!(tree.remove(&grandchild_id));
        // flatten order is now: root, child-a, child-b (2 items after child-a).
        assert!(tree.cursor <= tree.flatten().len().saturating_sub(1));

        assert!(tree.remove(&child_a_id));
        assert!(tree.selected().is_some());
    }

    #[test]
    fn registry_remove_deletes_agent() {
        use super::super::registry::AgentRegistry;
        let reg = AgentRegistry::new();
        let result = reg.register(super::super::registry::RegisterArgs {
            id: None,
            name: "tmp".to_string(),
            role: super::super::AgentRole::Root,
            parent_id: None,
            adapter: "claude".to_string(),
            repo: "r".to_string(),
            cwd: std::path::PathBuf::from("."),
            branch: None,
            initial_status: super::super::AgentStatus::Running,
        }).unwrap();
        assert!(!reg.snapshot().is_empty());
        reg.remove(&result.id);
        assert!(reg.snapshot().is_empty());
    }

    #[test]
    fn test_agent_counts_includes_collapsed() {
        let mut tree = AgentTree::new();
        let mut root = mock_root("root", "repo");
        root.status = AgentStatus::Running;
        let mut child = mock_child("child", "repo");
        child.status = AgentStatus::Running;
        root.children.push(child);
        root.expanded = false; // collapsed — child hidden from flatten()
        tree.add_root(root);

        let (running, blocked, total) = tree.agent_counts();
        assert_eq!(total, 2);
        assert_eq!(running, 2);
        assert_eq!(blocked, 0);
        assert_eq!(tree.flatten().len(), 1); // only root visible
    }

    #[test]
    fn test_agent_counts_counts_blocked() {
        let mut tree = AgentTree::new();
        let mut root = mock_root("root", "repo");
        root.status = AgentStatus::Blocked;
        let mut child = mock_child("child", "repo");
        child.status = AgentStatus::Running;
        root.children.push(child);
        tree.add_root(root);

        let (running, blocked, total) = tree.agent_counts();
        assert_eq!(total, 2);
        assert_eq!(running, 1);
        assert_eq!(blocked, 1);
    }
}
