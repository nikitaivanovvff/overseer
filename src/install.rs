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
    }

    Ok(())
}

fn install_file(file: &InstalledFile, config_dir: &std::path::Path) -> Result<()> {
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
                serde_json::from_str(&existing_raw).unwrap_or_else(|_| serde_json::json!({}));
            let overlay: serde_json::Value =
                serde_json::from_str(&file.content).context("adapter returned invalid JSON")?;
            settings::merge_hooks(&mut existing, &overlay);
            let out = serde_json::to_string_pretty(&existing)?;
            std::fs::write(&full_path, out + "\n")
                .with_context(|| format!("failed to write {}", full_path.display()))?;
            println!("merged   {}", full_path.display());
        }
    }
    Ok(())
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
                    serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}));
                settings::remove_hooks(&mut json);
                let out = serde_json::to_string_pretty(&json)?;
                std::fs::write(&full_path, out + "\n")
                    .with_context(|| format!("failed to write {}", full_path.display()))?;
                println!("updated  {} (removed overseer hooks)", full_path.display());
            }
        }
    }
    Ok(())
}

/// Deletes each of the adapter's `legacy_paths()` under `config_dir`, if present.
/// A path may be a file or a directory — either is removed outright (deletion is
/// the documented preference over leaving a stale "superseded" pointer behind).
fn remove_legacy_paths(adapter: &dyn AgentAdapter, config_dir: &std::path::Path) -> Result<()> {
    for path in adapter.legacy_paths() {
        let full_path = config_dir.join(&path);
        if full_path.is_dir() {
            std::fs::remove_dir_all(&full_path)
                .with_context(|| format!("failed to remove legacy {}", full_path.display()))?;
            println!("removed  {} (legacy)", full_path.display());
        } else if full_path.exists() {
            std::fs::remove_file(&full_path)
                .with_context(|| format!("failed to remove legacy {}", full_path.display()))?;
            println!("removed  {} (legacy)", full_path.display());
        }
    }
    Ok(())
}
