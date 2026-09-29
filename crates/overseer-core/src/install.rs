//! Install engine: executes an adapter's `install_files()` according to each
//! file's `MergeStrategy`, at the user level. Runs once at `overseer install`,
//! no socket needed.

use anyhow::{Context, Result};

use crate::agent::adapters::{adapter_for, AgentAdapter, InstalledFile, MergeStrategy};
use crate::settings;

pub fn run_install(agent_name: &str, uninstall: bool) -> Result<()> {
    let adapter = adapter_for(agent_name)
        .ok_or_else(|| anyhow::anyhow!("unknown adapter: '{agent_name}'"))?;

    let config_dir = adapter
        .user_config_dir()
        .ok_or_else(|| anyhow::anyhow!("could not resolve user config dir for '{agent_name}'"))?;

    if uninstall {
        for file in adapter.install_files() {
            uninstall_file(&file, &config_dir)?;
        }
        remove_legacy_paths(adapter.as_ref(), &config_dir)?;
        println!("uninstalled '{agent_name}' adapter");
    } else {
        for file in adapter.install_files() {
            install_file(&file, &config_dir)?;
        }
        // A fresh install must not leave a superseded layout (e.g. the old
        // single skills/overseer/) sitting alongside the new one.
        remove_legacy_paths(adapter.as_ref(), &config_dir)?;
        println!("installed '{agent_name}' adapter → config dir: {}", config_dir.display());
        if agent_name == "codex" {
            println!("Codex integration is experimental. In Codex, open /hooks, review and trust the Overseer hooks, then restart the session. Hook trust is never changed by Overseer.");
        }
    }

    Ok(())
}

fn install_file(file: &InstalledFile, config_dir: &std::path::Path) -> Result<()> {
    if matches!(file.merge, MergeStrategy::JsonArrayRemove { .. }) {
        return uninstall_file(file, config_dir);
    }
    let full_path = config_dir.join(&file.path);
    if let Some(parent) = full_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    match file.merge {
        MergeStrategy::Overwrite => {
            std::fs::write(&full_path, &file.content)
                .with_context(|| format!("failed to write {}", full_path.display()))?;
            println!("wrote    {}", full_path.display());
        }
        MergeStrategy::JsonMerge => {
            let existing_raw = if full_path.exists() {
                std::fs::read_to_string(&full_path)
                    .with_context(|| format!("failed to read {}", full_path.display()))?
            } else {
                "{}".to_string()
            };
            let mut existing: serde_json::Value =
                parse_settings(&existing_raw, &full_path)?;
            let overlay: serde_json::Value =
                serde_json::from_str(&file.content).context("adapter returned invalid JSON")?;
            settings::merge_hooks(&mut existing, &overlay);
            let out = serde_json::to_string_pretty(&existing)?;
            write_settings(&full_path, &(out + "\n"))
                .with_context(|| format!("failed to write {}", full_path.display()))?;
            println!("merged   {}", full_path.display());
        }
        MergeStrategy::JsonArrayMerge { key, ref entries } => {
            let existing_raw = if full_path.exists() {
                std::fs::read_to_string(&full_path)
                    .with_context(|| format!("failed to read {}", full_path.display()))?
            } else {
                "{}".to_string()
            };
            let mut existing: serde_json::Value =
                parse_settings(&existing_raw, &full_path)?;
            settings::merge_json_array(&mut existing, key, entries);
            let out = serde_json::to_string_pretty(&existing)?;
            write_settings(&full_path, &(out + "\n"))
                .with_context(|| format!("failed to write {}", full_path.display()))?;
            println!("merged   {}", full_path.display());
        }
        MergeStrategy::JsonArrayRemove { .. } => unreachable!("removal handled before creating paths"),
    }
    Ok(())
}

/// JSONC permits comments and trailing commas, but otherwise uses JSON syntax.
/// Replace extensions with whitespace so JSON validation still catches errors.
fn parse_settings(raw: &str, path: &std::path::Path) -> Result<serde_json::Value> {
    let mut bytes = raw.as_bytes().to_vec();
    if path.extension().is_some_and(|ext| ext == "jsonc") {
        let mut i = 0;
        let mut quoted = false;
        while i < bytes.len() {
            if quoted {
                match bytes[i] {
                    b'\\' => { i += 2; continue; }
                    b'"' => quoted = false,
                    _ => {}
                }
            } else if bytes[i] == b'"' {
                quoted = true;
            } else if bytes[i..].starts_with(b"//") {
                while i < bytes.len() && bytes[i] != b'\n' {
                    bytes[i] = b' '; i += 1;
                }
                continue;
            } else if bytes[i..].starts_with(b"/*") {
                let start = i;
                i += 2;
                while i + 1 < bytes.len() && !bytes[i..].starts_with(b"*/") { i += 1; }
                anyhow::ensure!(i + 1 < bytes.len(), "unterminated comment in {}", path.display());
                i += 2;
                for byte in &mut bytes[start..i] { if *byte != b'\n' { *byte = b' '; } }
                continue;
            }
            i += 1;
        }
        let mut i = 0;
        quoted = false;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' if quoted => { i += 2; continue; }
                b'"' => quoted = !quoted,
                b',' if !quoted => {
                    let next = bytes[i + 1..].iter().find(|b| !b.is_ascii_whitespace());
                    if matches!(next, Some(b'}' | b']')) { bytes[i] = b' '; }
                }
                _ => {}
            }
            i += 1;
        }
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid settings in {}; original file preserved", path.display()))?;
    anyhow::ensure!(value.is_object(), "settings in {} must be an object", path.display());
    Ok(value)
}

fn write_settings(path: &std::path::Path, content: &str) -> Result<()> {
    use std::io::Write;
    // Resolve an existing symlink so atomic replacement preserves that link.
    let target = if path.exists() { path.canonicalize()? } else { path.to_path_buf() };
    let temporary = target.with_file_name(format!(".overseer-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        if let Ok(metadata) = std::fs::metadata(&target) { file.set_permissions(metadata.permissions())?; }
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, &target)?;
        Ok(())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&temporary); }
    result
}

fn uninstall_file(file: &InstalledFile, config_dir: &std::path::Path) -> Result<()> {
    let full_path = config_dir.join(&file.path);
    match file.merge {
        MergeStrategy::Overwrite => {
            if full_path.exists() {
                std::fs::remove_file(&full_path)
                    .with_context(|| format!("failed to remove {}", full_path.display()))?;
                println!("removed  {}", full_path.display());
            }
        }
        MergeStrategy::JsonMerge => {
            if full_path.exists() {
                let raw = std::fs::read_to_string(&full_path)
                    .with_context(|| format!("failed to read {}", full_path.display()))?;
                let mut json: serde_json::Value =
                    parse_settings(&raw, &full_path)?;
                settings::remove_hooks(&mut json);
                let out = serde_json::to_string_pretty(&json)?;
                write_settings(&full_path, &(out + "\n"))
                    .with_context(|| format!("failed to write {}", full_path.display()))?;
                println!("updated  {} (removed overseer hooks)", full_path.display());
            }
        }
        MergeStrategy::JsonArrayMerge { key, ref entries }
        | MergeStrategy::JsonArrayRemove { key, ref entries } => {
            if full_path.exists() {
                let raw = std::fs::read_to_string(&full_path)
                    .with_context(|| format!("failed to read {}", full_path.display()))?;
                let mut json: serde_json::Value =
                    parse_settings(&raw, &full_path)?;
                settings::remove_json_array(&mut json, key, entries);
                let out = serde_json::to_string_pretty(&json)?;
                write_settings(&full_path, &(out + "\n"))
                    .with_context(|| format!("failed to write {}", full_path.display()))?;
                println!("updated  {} (removed overseer entries)", full_path.display());
            }
        }
    }
    Ok(())
}

/// Deletes only the exact owned files named by `legacy_paths()`. A directory
/// may contain user additions and must never be removed recursively.
fn remove_legacy_paths(adapter: &dyn AgentAdapter, config_dir: &std::path::Path) -> Result<()> {
    for path in adapter.legacy_paths() {
        let full_path = config_dir.join(&path);
        remove_legacy_file(&full_path)?;
    }
    Ok(())
}

fn remove_legacy_file(path: &std::path::Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(!metadata.is_dir(), "refusing to remove legacy directory {}; expected an owned file", path.display());
            std::fs::remove_file(path)
                .with_context(|| format!("failed to remove legacy {}", path.display()))?;
            println!("removed  {} (legacy)", path.display());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_cleanup_preserves_directories_and_symlink_targets() {
        let dir = std::env::temp_dir().join(format!("overseer-legacy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let user_file = dir.join("user.md");
        std::fs::write(&user_file, "user instructions").unwrap();
        assert!(remove_legacy_file(&dir).is_err());
        let link = dir.join("owned-link");
        std::os::unix::fs::symlink(&user_file, &link).unwrap();
        remove_legacy_file(&link).unwrap();
        assert!(!link.exists());
        assert_eq!(std::fs::read_to_string(&user_file).unwrap(), "user instructions");
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn instruction_migration_removes_only_exact_entries_without_creating_config() {
        let dir = std::env::temp_dir().join(format!("overseer-migrate-{}", uuid::Uuid::new_v4()));
        let file = InstalledFile {
            path: "opencode.jsonc".into(), content: String::new(),
            merge: MergeStrategy::JsonArrayRemove { key: "instructions", entries: vec!["overseer-root.md".into()] },
        };
        install_file(&file, &dir).unwrap();
        assert!(!dir.exists());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("opencode.jsonc");
        std::fs::write(&path, r#"{"model":"user-model","instructions":["overseer-root.md","user.md"]}"#).unwrap();
        install_file(&file, &dir).unwrap();
        uninstall_file(&file, &dir).unwrap();
        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value, serde_json::json!({"model":"user-model","instructions":["user.md"]}));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn codex_hooks_install_upgrade_and_uninstall_preserve_user_hooks() {
        use crate::agent::adapters::codex::CodexAdapter;
        let dir = std::env::temp_dir().join(format!("overseer-codex-install-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hooks.json");
        let original = serde_json::json!({"description":"user hooks", "hooks": {
            "Stop": [{"hooks":[{"type":"command","command":"echo user"}]}]
        }});
        std::fs::write(&path, original.to_string()).unwrap();
        let files = CodexAdapter::new().install_files();
        install_file(&files[0], &dir).unwrap();
        install_file(&files[0], &dir).unwrap();
        let installed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(installed["hooks"]["Stop"].as_array().unwrap().len(), 2);
        uninstall_file(&files[0], &dir).unwrap();
        let removed: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(removed, original);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn settings_merge_preserves_jsonc_and_rejects_invalid_input() {
        let dir = std::env::temp_dir().join(format!("overseer-install-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("opencode.jsonc");
        let file = InstalledFile { path: "opencode.jsonc".into(), content: String::new(), merge: MergeStrategy::JsonArrayMerge { key: "instructions", entries: vec!["ours.md".into()] } };
        std::fs::write(&path, "{ // comment\n \"model\": \"https://example/*literal*/\", /* block */ \"instructions\": [\"user.md\",], }").unwrap();
        install_file(&file, &dir).unwrap();
        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["model"], "https://example/*literal*/");
        assert_eq!(value["instructions"], serde_json::json!(["user.md", "ours.md"]));
        uninstall_file(&file, &dir).unwrap();
        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["instructions"], serde_json::json!(["user.md"]));
        for bad in ["{broken", "{/* unterminated", "[]"] {
            std::fs::write(&path, bad).unwrap();
            assert!(install_file(&file, &dir).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), bad);
            assert!(uninstall_file(&file, &dir).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), bad);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
