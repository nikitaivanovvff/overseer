/// Pure functions for merging/removing Overseer hook entries in Claude Code settings.json.
///
/// The `_overseer: true` sentinel on each hook group marks entries we own, so
/// `--uninstall` can remove only ours without touching user settings.
use serde_json::{json, Value};

/// Substrings that have appeared together in every generation of the
/// Overseer-authored SessionStart printf command, from before the
/// `_overseer: true` tagging convention existed through the current
/// role-specific-skill text. An entry matching both, untagged, is a relic
/// from an install predating the tag — not something a user wrote, since
/// no user hook would reference our own skill-following instructions.
///
/// Confirmed present in every historical variant (see git log on this file
/// and on src/agent/adapters/claude.rs):
///   - "...Follow the overseer skill.\n" "$OVERSEER_ROLE" || true
///   - "...Follow the overseer skill.\n" "$OVERSEER_ROLE"; {post_tool_cmd}; } || true
///   - "...Follow the overseer skill.\n" "$OVERSEER_ROLE"; {running_cmd}; } || true
///   - "...Follow the overseer-%s skill.\n" "$OVERSEER_ROLE" "$OVERSEER_ROLE"; {running_cmd}; } || true
///   - "...Follow the overseer-%s skill.\n" "$OVERSEER_ROLE" "$OVERSEER_ROLE"; {session_start_status_cmd}; } || true
const LEGACY_SIGNATURE_PARTS: [&str; 2] = ["OVERSEER_AGENT_ID", "Follow the overseer"];

/// A second, independent legacy signature: found live on a real machine
/// (2026-07-09) as *untagged* `PostToolUse`/`Stop` duplicates sitting
/// alongside the correctly-tagged current entry — bare `overseer status
/// running` / `overseer status done` commands with none of the current
/// flags (`--from-hook`, etc). These predate not just the `_overseer` tag
/// but also the `--from-hook` classification and the "no hook ever pushes
/// done" design (AGENTS.md Cleanup) — a bare `overseer status done` firing
/// on *every* `Stop` event raced the correctly-tagged `status idle` push
/// and intermittently won, which is exactly what made a live root's PTY
/// look like it had silently died (it hadn't — its status just got forced
/// to `done`, which the UI and exit-sweep both treat as terminal). Unlike
/// the SessionStart-flavored signature above, this one carries no
/// human-readable text to match — the invariant that actually holds across
/// every generation of every hook we've ever shipped is simpler: it invokes
/// our own binary's `status` subcommand at all. No user-authored hook has
/// reason to invoke *our* CLI's `status` subcommand, so matching on that
/// literal invocation is both necessary and sufficient, and naturally
/// covers any future legacy variant too without needing a new signature
/// added every time the hook text's surrounding shape changes.
fn invokes_overseer_status_subcommand(cmd: &str) -> bool {
    cmd.contains("overseer status ")
}

/// True if `entry` is an Overseer-authored hook entry: either tagged with
/// `_overseer: true` (current convention), or — for installs that predate
/// that tag — carrying a legacy command signature every generation of our
/// hook text has contained. OpenCode is exempt from the legacy
/// check since it never emitted JSON hook entries like this.
fn is_overseer_entry(entry: &Value) -> bool {
    if entry.get("_overseer").and_then(|v| v.as_bool()) == Some(true) {
        return true;
    }
    let Some(hooks) = entry.get("hooks").and_then(|h| h.as_array()) else {
        return false;
    };
    hooks.iter().any(|h| {
        h.get("command")
            .and_then(|c| c.as_str())
            .map(|cmd| {
                LEGACY_SIGNATURE_PARTS.iter().all(|part| cmd.contains(part))
                    || invokes_overseer_status_subcommand(cmd)
            })
            .unwrap_or(false)
    })
}

/// Deep-merges `overlay` hooks into `existing` settings.
///
/// For each hook event in `overlay["hooks"]`:
/// - Removes any existing entries with `_overseer: true`, plus any untagged
///   entries matching the legacy Overseer signature (idempotent re-run, and
///   convergent for installs from before the tagging convention existed).
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

        // Drop our old entries (tagged or legacy-signature), keep the user's.
        let mut kept: Vec<Value> = current.into_iter().filter(|e| !is_overseer_entry(e)).collect();
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

/// Removes all Overseer-managed hook entries from `settings`: those tagged
/// `_overseer: true`, plus untagged legacy entries matching the pre-tagging
/// signature (see `is_overseer_entry`). Hook event keys that become empty
/// arrays are removed entirely.
pub fn remove_hooks(settings: &mut Value) {
    let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return;
    };

    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        if let Some(arr) = hooks.get_mut(&event).and_then(|v| v.as_array_mut()) {
            arr.retain(|e| !is_overseer_entry(e));
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

    /// The very first hook command we ever shipped, from before the
    /// `_overseer: true` tag existed — no `status` push, single shared skill.
    fn legacy_untagged_entry() -> Value {
        json!({
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": r#"[ -n "$OVERSEER_AGENT_ID" ] && printf 'You are managed by Overseer (role: %s). Follow the overseer skill.\n' "$OVERSEER_ROLE" || true"#
            }]
        })
    }

    #[test]
    fn merge_removes_untagged_legacy_overseer_duplicates() {
        let mut settings = json!({
            "hooks": {
                "SessionStart": [legacy_untagged_entry(), legacy_untagged_entry()]
            }
        });
        merge_hooks(&mut settings, &overlay_session_start());
        let arr = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "legacy untagged duplicates should be dropped, not accumulated");
        assert!(
            arr[0].get("_overseer") == Some(&json!(true)),
            "surviving entry should be the freshly tagged one"
        );
    }

    #[test]
    fn merge_leaves_genuine_user_hook_alone() {
        // Unrelated to Overseer entirely: untagged, and doesn't mention either
        // signature substring, so it must survive the legacy-signature filter
        // exactly like it already survives the `_overseer` tag filter.
        let unrelated_user_hook = user_entry("echo hello && ./scripts/lint.sh");
        let mut settings = json!({
            "hooks": {
                "PostToolUse": [unrelated_user_hook.clone()]
            }
        });
        merge_hooks(&mut settings, &overlay());
        let arr = settings["hooks"]["PostToolUse"].as_array().unwrap();
        // User entry preserved, plus the overseer entry appended.
        assert_eq!(arr.len(), 2, "user entry must survive merge: {arr:?}");
        assert!(arr.contains(&unrelated_user_hook));
    }

    #[test]
    fn merge_still_replaces_currently_tagged_entry() {
        let mut settings = json!({
            "hooks": {
                "SessionStart": [overseer_entry("status running --from-hook --adapter claude")]
            }
        });
        merge_hooks(&mut settings, &overlay_session_start());
        let arr = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "stale tagged entry should be replaced, not duplicated");
        let cmd = arr[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("OVERSEER_ROLE"), "should be the fresh role-branching command: {cmd}");
    }

    /// The exact shape found live on a real machine: an untagged `Stop`
    /// duplicate pushing a bare `status done` on every turn, no `--from-hook`
    /// or any other current-generation flag — this is the one that raced a
    /// correctly-tagged `status idle` push and intermittently won, making a
    /// perfectly healthy root's PTY look like it had silently died.
    fn legacy_bare_status_push_entry(cmd: &str) -> Value {
        json!({
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": format!("/Users/x/bin/overseer status {cmd}")
            }]
        })
    }

    #[test]
    fn merge_removes_legacy_bare_status_push_duplicates_on_stop() {
        let mut settings = json!({
            "hooks": {
                "Stop": [
                    legacy_bare_status_push_entry("done"),
                    legacy_bare_status_push_entry("done"),
                ]
            }
        });
        let overlay = json!({
            "hooks": {
                "Stop": [overseer_entry("status idle --from-hook")]
            }
        });
        merge_hooks(&mut settings, &overlay);
        let arr = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "legacy bare `status done` duplicates must not survive alongside the tagged idle push: {arr:?}");
        assert!(arr[0].get("_overseer") == Some(&json!(true)));
    }

    #[test]
    fn remove_also_cleans_legacy_bare_status_push_entries() {
        let mut settings = json!({
            "hooks": {
                "PostToolUse": [legacy_bare_status_push_entry("running"), user_entry("echo hi")]
            }
        });
        remove_hooks(&mut settings);
        let arr = settings["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "legacy entry should be gone, user entry kept");
        assert_eq!(arr[0]["hooks"][0]["command"], "echo hi");
    }

    fn overlay_session_start() -> Value {
        json!({
            "hooks": {
                "SessionStart": [overseer_entry(
                    r#"[ -n "$OVERSEER_AGENT_ID" ] && { printf 'You are managed by Overseer (role: %s). Follow the overseer-%s skill.\n' "$OVERSEER_ROLE" "$OVERSEER_ROLE"; if [ "$OVERSEER_ROLE" = "root" ]; then overseer status idle; else overseer status running; fi; } || true"#
                )]
            }
        })
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
    fn remove_also_cleans_untagged_legacy_entries() {
        let mut settings = json!({
            "hooks": {
                "SessionStart": [legacy_untagged_entry(), user_entry("echo hi")]
            }
        });
        remove_hooks(&mut settings);
        let arr = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "legacy entry should be gone, user entry kept");
        assert_eq!(arr[0]["hooks"][0]["command"], "echo hi");
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
