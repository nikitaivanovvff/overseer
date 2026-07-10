//! `[keybindings]` — tree-focus bindings only (PHASE5B.md Task 2). Pane-focus
//! interception (`Ctrl-h`) is deliberately **not** configurable — see
//! AGENTS.md's keybinding house style; making it remappable would invite a
//! user to steal a key their agent's own TUI needs.
//!
//! Kept free of any input-backend dependency (no `crossterm` here) — parsing
//! a config string into a `KeyBinding` and converting a live `KeyEvent` into
//! one are two different concerns; the latter lives in `tui.rs`, which
//! already owns `crossterm`.

use std::collections::HashMap;

use serde::Deserialize;

/// One key, in the vocabulary a config file can express: a bare character
/// (`"j"`, `"D"`, `"/"`, `"?"`) or `ctrl-<char>` (case-insensitive on the
/// `ctrl-` prefix and the modified letter itself, since Ctrl+A and Ctrl+a
/// are physically the same keystroke). `Enter`/`Esc` round out what
/// `handle_tree_key` might plausibly bind, though no default action uses
/// them today (`Enter` is a fixed `jump_in` alias, never a configurable
/// target).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyBinding {
    Char(char),
    Ctrl(char),
    Enter,
    Esc,
}

/// Parses one `[keybindings]` value. `None` for anything that doesn't match
/// the supported syntax — callers treat that as "keep the default" plus a
/// warning, never a hard error (a config problem must never block startup).
pub fn parse_binding(s: &str) -> Option<KeyBinding> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(rest) = strip_ctrl_prefix(s) {
        let mut chars = rest.chars();
        let c = chars.next()?;
        if chars.next().is_some() {
            return None; // "ctrl-" must be followed by exactly one char
        }
        return Some(KeyBinding::Ctrl(c.to_ascii_lowercase()));
    }
    match s.to_lowercase().as_str() {
        "enter" => return Some(KeyBinding::Enter),
        "esc" | "escape" => return Some(KeyBinding::Esc),
        "space" => return Some(KeyBinding::Char(' ')),
        _ => {}
    }
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None; // exactly one char, case preserved ("D" != "d")
    }
    Some(KeyBinding::Char(c))
}

/// The human-readable form the help popup (PHASE5B.md Task 3) prints next to
/// each action — the inverse of `parse_binding` (modulo case normalization
/// already applied when the binding was built).
impl std::fmt::Display for KeyBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyBinding::Char(' ') => write!(f, "space"),
            KeyBinding::Char(c) => write!(f, "{c}"),
            KeyBinding::Ctrl(c) => write!(f, "ctrl-{c}"),
            KeyBinding::Enter => write!(f, "enter"),
            KeyBinding::Esc => write!(f, "esc"),
        }
    }
}

fn strip_ctrl_prefix(s: &str) -> Option<&str> {
    s.strip_prefix("ctrl-").or_else(|| s.strip_prefix("Ctrl-")).or_else(|| s.strip_prefix("CTRL-"))
}

/// Every tree-focus action a key can be bound to. `Shutdown`/`ToggleExpand`
/// aren't in PHASE5B.md's original sample `[keybindings]` block (written
/// before `Q`/`<space>` existed as bindable concepts) — included anyway for
/// completeness, since the TOML comment's intent ("tree-focus bindings only,
/// all optional") is clearly "every one of them", not literally just the
/// ones the sample happened to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    NavDown,
    NavUp,
    ToggleExpand,
    JumpIn,
    SpawnRoot,
    SpawnChild,
    Drop,
    DropRecursive,
    Quit,
    Shutdown,
    Search,
    Help,
}

impl Action {
    /// Every action, in a fixed declaration order — later entries win a
    /// same-key collision, both in `Keybindings::action_for_key`'s lookup
    /// and in the startup-warning pass that reports one. `label` is what the
    /// help popup prints next to each row.
    pub const ALL: [Action; 12] = [
        Action::NavDown,
        Action::NavUp,
        Action::ToggleExpand,
        Action::JumpIn,
        Action::SpawnRoot,
        Action::SpawnChild,
        Action::Drop,
        Action::DropRecursive,
        Action::Quit,
        Action::Shutdown,
        Action::Search,
        Action::Help,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Action::NavDown => "navigate down",
            Action::NavUp => "navigate up",
            Action::ToggleExpand => "fold/unfold",
            Action::JumpIn => "jump in",
            Action::SpawnRoot => "new workspace",
            Action::SpawnChild => "spawn child",
            Action::Drop => "drop",
            Action::DropRecursive => "recursive drop",
            Action::Quit => "quit (detach)",
            Action::Shutdown => "shutdown (kill switch)",
            Action::Search => "search",
            Action::Help => "help",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Keybindings {
    pub nav_down: KeyBinding,
    pub nav_up: KeyBinding,
    pub toggle_expand: KeyBinding,
    pub jump_in: KeyBinding,
    pub spawn_root: KeyBinding,
    pub spawn_child: KeyBinding,
    pub drop: KeyBinding,
    pub drop_recursive: KeyBinding,
    pub quit: KeyBinding,
    pub shutdown: KeyBinding,
    pub search: KeyBinding,
    pub help: KeyBinding,
}

impl Default for Keybindings {
    fn default() -> Self {
        Self {
            nav_down: KeyBinding::Char('j'),
            nav_up: KeyBinding::Char('k'),
            toggle_expand: KeyBinding::Char(' '),
            jump_in: KeyBinding::Ctrl('l'),
            spawn_root: KeyBinding::Char('n'),
            spawn_child: KeyBinding::Char('s'),
            drop: KeyBinding::Char('d'),
            drop_recursive: KeyBinding::Char('D'),
            quit: KeyBinding::Char('q'),
            shutdown: KeyBinding::Char('Q'),
            search: KeyBinding::Char('/'),
            help: KeyBinding::Char('?'),
        }
    }
}

impl Keybindings {
    /// The key currently bound to `action` — public so the help popup
    /// (PHASE5B.md Task 3) can build its rows straight from the live struct,
    /// never a hardcoded string list.
    pub fn get(&self, action: Action) -> KeyBinding {
        match action {
            Action::NavDown => self.nav_down,
            Action::NavUp => self.nav_up,
            Action::ToggleExpand => self.toggle_expand,
            Action::JumpIn => self.jump_in,
            Action::SpawnRoot => self.spawn_root,
            Action::SpawnChild => self.spawn_child,
            Action::Drop => self.drop,
            Action::DropRecursive => self.drop_recursive,
            Action::Quit => self.quit,
            Action::Shutdown => self.shutdown,
            Action::Search => self.search,
            Action::Help => self.help,
        }
    }

    fn set(&mut self, action: Action, binding: KeyBinding) {
        match action {
            Action::NavDown => self.nav_down = binding,
            Action::NavUp => self.nav_up = binding,
            Action::ToggleExpand => self.toggle_expand = binding,
            Action::JumpIn => self.jump_in = binding,
            Action::SpawnRoot => self.spawn_root = binding,
            Action::SpawnChild => self.spawn_child = binding,
            Action::Drop => self.drop = binding,
            Action::DropRecursive => self.drop_recursive = binding,
            Action::Quit => self.quit = binding,
            Action::Shutdown => self.shutdown = binding,
            Action::Search => self.search = binding,
            Action::Help => self.help = binding,
        }
    }

    /// The action bound to `key`, if any. On a collision (two actions
    /// sharing a key — warned about at load time), the action declared later
    /// in `Action::ALL` wins, deterministically.
    pub fn action_for_key(&self, key: KeyBinding) -> Option<Action> {
        let mut found = None;
        for action in Action::ALL {
            if self.get(action) == key {
                found = Some(action);
            }
        }
        found
    }

    /// Action name -> TOML key, for parsing `[keybindings]` and for
    /// diagnosing an unrecognized action name.
    fn action_by_name(name: &str) -> Option<Action> {
        Some(match name {
            "nav_down" => Action::NavDown,
            "nav_up" => Action::NavUp,
            "toggle_expand" => Action::ToggleExpand,
            "jump_in" => Action::JumpIn,
            "spawn_root" => Action::SpawnRoot,
            "spawn_child" => Action::SpawnChild,
            "drop" => Action::Drop,
            "drop_recursive" => Action::DropRecursive,
            "quit" => Action::Quit,
            "shutdown" => Action::Shutdown,
            "search" => Action::Search,
            "help" => Action::Help,
            _ => return None,
        })
    }

    /// Builds from the raw `[keybindings]` table: starts from defaults,
    /// applies each recognized+parseable override, and warns (stderr, never
    /// an error — a config problem must never block startup) on an unknown
    /// action name, an unparseable value, or two actions landing on the same
    /// key.
    fn from_raw(raw: &HashMap<String, String>) -> Self {
        let mut bindings = Self::default();
        for (name, value) in raw {
            let Some(action) = Self::action_by_name(name) else {
                eprintln!("overseer: unknown [keybindings] action '{name}' — ignoring");
                continue;
            };
            let Some(binding) = parse_binding(value) else {
                eprintln!("overseer: unrecognized [keybindings] value '{value}' for '{name}' — keeping the default");
                continue;
            };
            bindings.set(action, binding);
        }
        bindings.warn_on_collisions();
        bindings
    }

    fn warn_on_collisions(&self) {
        for i in 0..Action::ALL.len() {
            for j in (i + 1)..Action::ALL.len() {
                let (a, b) = (Action::ALL[i], Action::ALL[j]);
                if self.get(a) == self.get(b) {
                    eprintln!(
                        "overseer: keybinding collision — '{}' and '{}' are both bound to the same key; '{}' wins",
                        a.label(),
                        b.label(),
                        b.label()
                    );
                }
            }
        }
    }
}

impl<'de> Deserialize<'de> for Keybindings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = HashMap::<String, String>::deserialize(deserializer)?;
        Ok(Keybindings::from_raw(&raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_binding ─────────────────────────────────────────────────────────

    #[test]
    fn parse_binding_single_chars_case_sensitive() {
        assert_eq!(parse_binding("j"), Some(KeyBinding::Char('j')));
        assert_eq!(parse_binding("D"), Some(KeyBinding::Char('D')));
        assert_eq!(parse_binding("/"), Some(KeyBinding::Char('/')));
        assert_eq!(parse_binding("?"), Some(KeyBinding::Char('?')));
    }

    #[test]
    fn parse_binding_ctrl_combos() {
        assert_eq!(parse_binding("ctrl-l"), Some(KeyBinding::Ctrl('l')));
        assert_eq!(parse_binding("Ctrl-L"), Some(KeyBinding::Ctrl('l')));
        assert_eq!(parse_binding("CTRL-x"), Some(KeyBinding::Ctrl('x')));
    }

    #[test]
    fn parse_binding_enter_esc_space_names() {
        assert_eq!(parse_binding("enter"), Some(KeyBinding::Enter));
        assert_eq!(parse_binding("Enter"), Some(KeyBinding::Enter));
        assert_eq!(parse_binding("esc"), Some(KeyBinding::Esc));
        assert_eq!(parse_binding("escape"), Some(KeyBinding::Esc));
        assert_eq!(parse_binding("space"), Some(KeyBinding::Char(' ')));
    }

    #[test]
    fn parse_binding_garbage_is_none() {
        assert_eq!(parse_binding(""), None);
        assert_eq!(parse_binding("too-long"), None);
        assert_eq!(parse_binding("ctrl-"), None);
        assert_eq!(parse_binding("ctrl-xy"), None);
    }

    // ── defaulting ────────────────────────────────────────────────────────────

    #[test]
    fn default_matches_the_documented_defaults() {
        let kb = Keybindings::default();
        assert_eq!(kb.nav_down, KeyBinding::Char('j'));
        assert_eq!(kb.nav_up, KeyBinding::Char('k'));
        assert_eq!(kb.jump_in, KeyBinding::Ctrl('l'));
        assert_eq!(kb.spawn_root, KeyBinding::Char('n'));
        assert_eq!(kb.spawn_child, KeyBinding::Char('s'));
        assert_eq!(kb.drop, KeyBinding::Char('d'));
        assert_eq!(kb.drop_recursive, KeyBinding::Char('D'));
        assert_eq!(kb.quit, KeyBinding::Char('q'));
        assert_eq!(kb.shutdown, KeyBinding::Char('Q'));
        assert_eq!(kb.search, KeyBinding::Char('/'));
        assert_eq!(kb.help, KeyBinding::Char('?'));
    }

    #[test]
    fn from_raw_empty_map_is_the_default() {
        let kb = Keybindings::from_raw(&HashMap::new());
        assert_eq!(kb.nav_down, KeyBinding::Char('j'));
    }

    #[test]
    fn from_raw_overrides_one_action_keeps_the_rest_default() {
        let mut raw = HashMap::new();
        raw.insert("spawn_root".to_string(), "a".to_string());
        let kb = Keybindings::from_raw(&raw);
        assert_eq!(kb.spawn_root, KeyBinding::Char('a'));
        assert_eq!(kb.nav_down, KeyBinding::Char('j')); // untouched
    }

    #[test]
    fn from_raw_unknown_action_name_is_ignored_not_fatal() {
        let mut raw = HashMap::new();
        raw.insert("saerch".to_string(), "/".to_string()); // typo
        let kb = Keybindings::from_raw(&raw);
        assert_eq!(kb.search, KeyBinding::Char('/')); // still the default
    }

    #[test]
    fn from_raw_unparseable_value_keeps_the_default() {
        let mut raw = HashMap::new();
        raw.insert("quit".to_string(), "way-too-long".to_string());
        let kb = Keybindings::from_raw(&raw);
        assert_eq!(kb.quit, KeyBinding::Char('q'));
    }

    // ── action_for_key / collisions ───────────────────────────────────────────

    #[test]
    fn action_for_key_finds_the_bound_action() {
        let kb = Keybindings::default();
        assert_eq!(kb.action_for_key(KeyBinding::Char('j')), Some(Action::NavDown));
        assert_eq!(kb.action_for_key(KeyBinding::Ctrl('l')), Some(Action::JumpIn));
        assert_eq!(kb.action_for_key(KeyBinding::Char('z')), None);
    }

    #[test]
    fn a_later_action_wins_a_collision() {
        // Action::ALL declares Quit before Shutdown — remap shutdown onto
        // quit's key and confirm the lookup resolves to Shutdown (later).
        let mut raw = HashMap::new();
        raw.insert("shutdown".to_string(), "q".to_string());
        let kb = Keybindings::from_raw(&raw);
        assert_eq!(kb.action_for_key(KeyBinding::Char('q')), Some(Action::Shutdown));
    }

    #[test]
    fn remapped_action_is_reachable_at_its_new_key_and_gone_from_the_old_one() {
        let mut raw = HashMap::new();
        raw.insert("spawn_root".to_string(), "a".to_string());
        let kb = Keybindings::from_raw(&raw);
        assert_eq!(kb.action_for_key(KeyBinding::Char('a')), Some(Action::SpawnRoot));
        // 'n' is now unbound (nothing else claims it in this remap).
        assert_eq!(kb.action_for_key(KeyBinding::Char('n')), None);
    }
}
