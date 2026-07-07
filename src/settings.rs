/// Pure functions for merging/removing Overseer hook entries in Claude Code settings.json.
///
/// The `_overseer: true` sentinel on each hook group marks entries we own, so
/// `--uninstall` can remove only ours without touching user settings.
use serde_json::{json, Value};

/// Deep-merges `overlay` hooks into `existing` settings.
///
/// For each hook event in `overlay["hooks"]`:
/// - Removes any existing entries with `_overseer: true` (idempotent re-run).
/// - Appends the overlay entries.
///
/// Unrelated keys in `existing` are untouched.
pub fn merge_hooks(existing: &mut Value, overlay: &Value) {
    let Some(overlay_hooks) = overlay["hooks"].as_object() else {
        return;
    };
    let existing_hooks = existing
        .as_object_mut()
        .and_then(|o| {
            o.entry("hooks").or_insert_with(|| json!({}));
            o.get_mut("hooks")
        })
        .and_then(|h| h.as_object_mut());

    let Some(existing_hooks) = existing_hooks else {
        return;
    };

    for (event, new_entries) in overlay_hooks {
        let Some(new_arr) = new_entries.as_array() else {
            continue;
        };
        let current = existing_hooks
            .entry(event.clone())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .cloned()
            .unwrap_or_default();

        // Drop our old entries, keep the user's.
        let mut kept: Vec<Value> = current
            .into_iter()
            .filter(|e| e.get("_overseer").and_then(|v| v.as_bool()) != Some(true))
            .collect();
        kept.extend(new_arr.clone());
        existing_hooks.insert(event.clone(), Value::Array(kept));
    }
}

/// Merges `entries` into `existing[key]`'s array (creating it if absent),
/// appending any not already present — idempotent on repeated installs.
/// Unlike `merge_hooks`, array elements here are bare strings (e.g. opencode's
/// `instructions` file paths), so there's no room for an `_overseer` sentinel
/// to mark ownership; `remove_json_array` instead relies on the caller
/// passing back the exact same `entries` it originally merged in.
pub fn merge_json_array(existing: &mut Value, key: &str, entries: &[String]) {
    let Some(obj) = existing.as_object_mut() else { return };
    let arr = obj.entry(key).or_insert_with(|| json!([])).as_array_mut();
    let Some(arr) = arr else { return };
    for entry in entries {
        if !arr.iter().any(|e| e.as_str() == Some(entry.as_str())) {
            arr.push(json!(entry));
        }
    }
}

/// Removes exactly `entries` from `existing[key]`'s array, if present.
/// Removes the key entirely if the array becomes empty as a result.
pub fn remove_json_array(existing: &mut Value, key: &str, entries: &[String]) {
    let Some(obj) = existing.as_object_mut() else { return };
    if let Some(arr) = obj.get_mut(key).and_then(|v| v.as_array_mut()) {
        arr.retain(|e| !entries.iter().any(|entry| e.as_str() == Some(entry.as_str())));
    }
    let is_empty = obj.get(key).and_then(|v| v.as_array()).map(|a| a.is_empty()).unwrap_or(false);
    if is_empty {
        obj.remove(key);
    }
}

/// Removes all Overseer-managed hook entries (those with `_overseer: true`) from
/// `settings`. Hook event keys that become empty arrays are removed entirely.
pub fn remove_hooks(settings: &mut Value) {
    let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return;
    };

    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        if let Some(arr) = hooks.get_mut(&event).and_then(|v| v.as_array_mut()) {
            arr.retain(|e| e.get("_overseer").and_then(|v| v.as_bool()) != Some(true));
        }
        // Remove the key if the array is now empty.
        let is_empty = hooks
            .get(&event)
            .and_then(|v| v.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(false);
        if is_empty {
            hooks.remove(&event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn overseer_entry(cmd: &str) -> Value {
        json!({
            "matcher": "",
            "_overseer": true,
            "hooks": [{"type": "command", "command": cmd}]
        })
    }

    fn user_entry(cmd: &str) -> Value {
        json!({
            "matcher": "Bash",
            "hooks": [{"type": "command", "command": cmd}]
        })
    }

    fn overlay() -> Value {
        json!({
            "hooks": {
                "PostToolUse": [overseer_entry("/bin/overseer status running")],
                "Stop": [overseer_entry("/bin/overseer status done")]
            }
        })
    }

    // --- merge_hooks ---

    #[test]
    fn merge_into_empty_settings_adds_hooks() {
        let mut settings = json!({});
        merge_hooks(&mut settings, &overlay());
        assert!(settings["hooks"]["PostToolUse"].is_array());
        assert_eq!(settings["hooks"]["PostToolUse"].as_array().unwrap().len(), 1);
        assert!(settings["hooks"]["Stop"].is_array());
    }

    #[test]
    fn merge_preserves_unrelated_user_keys() {
        let mut settings = json!({"theme": "dark", "fontSize": 14});
        merge_hooks(&mut settings, &overlay());
        assert_eq!(settings["theme"], "dark");
        assert_eq!(settings["fontSize"], 14);
    }

    #[test]
    fn merge_preserves_user_hooks() {
        let mut settings = json!({
            "hooks": {
                "PostToolUse": [user_entry("echo hello")]
            }
        });
        merge_hooks(&mut settings, &overlay());
        let arr = settings["hooks"]["PostToolUse"].as_array().unwrap();
        // user entry preserved + overseer entry appended
        assert_eq!(arr.len(), 2);
        assert!(arr.iter().any(|e| e.get("_overseer").is_none()), "user entry lost");
        assert!(
            arr.iter().any(|e| e.get("_overseer") == Some(&json!(true))),
            "overseer entry missing"
        );
    }

    #[test]
    fn merge_is_idempotent() {
        let mut settings = json!({});
        merge_hooks(&mut settings, &overlay());
        merge_hooks(&mut settings, &overlay());
        let arr = settings["hooks"]["PostToolUse"].as_array().unwrap();
        // Second merge should not duplicate — old _overseer entries are removed first.
        assert_eq!(arr.len(), 1, "idempotent merge should not duplicate entries");
    }

    #[test]
    fn merge_replaces_old_overseer_entry_with_new() {
        let mut settings = json!({});
        let old = json!({
            "hooks": {
                "Stop": [overseer_entry("/old/overseer status done")]
            }
        });
        merge_hooks(&mut settings, &old);
        let new = json!({
            "hooks": {
                "Stop": [overseer_entry("/new/overseer status done")]
            }
        });
        merge_hooks(&mut settings, &new);
        let arr = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let cmd = arr[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("/new/"), "old entry was not replaced");
    }

    // --- remove_hooks ---

    #[test]
    fn remove_deletes_overseer_entries() {
        let mut settings = json!({});
        merge_hooks(&mut settings, &overlay());
        remove_hooks(&mut settings);
        let hooks = settings["hooks"].as_object().unwrap();
        assert!(!hooks.contains_key("PostToolUse"), "PostToolUse should be gone");
        assert!(!hooks.contains_key("Stop"), "Stop should be gone");
    }

    #[test]
    fn remove_keeps_user_entries() {
        let mut settings = json!({
            "hooks": {
                "PostToolUse": [user_entry("echo hi")]
            }
        });
        merge_hooks(&mut settings, &overlay());
        remove_hooks(&mut settings);
        let arr = settings["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "user entry should remain after uninstall");
        assert!(arr[0].get("_overseer").is_none(), "remaining entry should not have _overseer");
    }

    #[test]
    fn remove_is_noop_on_empty_settings() {
        let mut settings = json!({});
        remove_hooks(&mut settings); // must not panic
        assert!(settings.as_object().unwrap().is_empty());
    }

    #[test]
    fn remove_then_merge_restores_clean_state() {
        let mut settings = json!({});
        merge_hooks(&mut settings, &overlay());
        remove_hooks(&mut settings);
        merge_hooks(&mut settings, &overlay());
        let arr = settings["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
    }

    // ── merge_json_array / remove_json_array (HARNESSES.md: opencode's `instructions`) ──

    #[test]
    fn json_array_merge_creates_the_key_when_absent() {
        let mut cfg = json!({});
        merge_json_array(&mut cfg, "instructions", &["overseer-root.md".to_string()]);
        assert_eq!(cfg["instructions"], json!(["overseer-root.md"]));
    }

    #[test]
    fn json_array_merge_appends_to_an_existing_array_without_disturbing_it() {
        let mut cfg = json!({"instructions": ["user-notes.md"]});
        merge_json_array(&mut cfg, "instructions", &["overseer-root.md".to_string()]);
        assert_eq!(cfg["instructions"], json!(["user-notes.md", "overseer-root.md"]));
    }

    #[test]
    fn json_array_merge_is_idempotent() {
        let mut cfg = json!({"instructions": ["user-notes.md"]});
        merge_json_array(&mut cfg, "instructions", &["overseer-root.md".to_string()]);
        merge_json_array(&mut cfg, "instructions", &["overseer-root.md".to_string()]);
        assert_eq!(cfg["instructions"], json!(["user-notes.md", "overseer-root.md"]));
    }

    #[test]
    fn json_array_remove_keeps_the_users_entries() {
        let mut cfg = json!({"instructions": ["user-notes.md", "overseer-root.md"]});
        remove_json_array(&mut cfg, "instructions", &["overseer-root.md".to_string()]);
        assert_eq!(cfg["instructions"], json!(["user-notes.md"]));
    }

    #[test]
    fn json_array_remove_drops_the_key_entirely_once_empty() {
        let mut cfg = json!({"instructions": ["overseer-root.md"]});
        remove_json_array(&mut cfg, "instructions", &["overseer-root.md".to_string()]);
        assert!(cfg.get("instructions").is_none());
    }

    #[test]
    fn json_array_remove_is_noop_on_empty_settings() {
        let mut cfg = json!({});
        remove_json_array(&mut cfg, "instructions", &["overseer-root.md".to_string()]); // must not panic
        assert!(cfg.as_object().unwrap().is_empty());
    }
}
