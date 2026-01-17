//! Configuration management command.
//!
//! Provides CLI access to the layered configuration system:
//! - Show current merged configuration
//! - Get/set individual config values
//! - List all available options
//! - Open config in editor
//! - Show config file paths

#![allow(clippy::default_trait_access)]

use crate::cli::ConfigArgs;
use crate::config::{
    discover_beads_dir, id_config_from_layer, load_config, load_project_config, load_user_config,
    resolve_actor, CliOverrides, ConfigLayer,
};
use crate::error::Result;
use serde_json::json;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Execute the config command.
///
/// # Errors
///
/// Returns an error if config cannot be loaded or operations fail.
pub fn execute(args: &ConfigArgs, json_mode: bool, overrides: &CliOverrides) -> Result<()> {
    // Handle path display first (doesn't need beads dir)
    if args.path {
        return show_paths(json_mode);
    }

    // Handle edit (doesn't need beads dir)
    if args.edit {
        return edit_config();
    }

    // Handle list (doesn't need beads dir)
    if args.list {
        return list_options(json_mode);
    }

    // Handle set (modifies user config)
    if let Some(ref kv) = args.set {
        return set_config_value(kv, json_mode);
    }

    // Try to discover beads dir for merged config
    let beads_dir = discover_beads_dir(None).ok();

    // Handle get specific key
    if let Some(ref key) = args.get {
        return get_config_value(key, beads_dir.as_ref(), overrides, json_mode);
    }

    // Show merged config (or subset)
    show_config(beads_dir.as_ref(), overrides, args, json_mode)
}

/// Show config file paths.
fn show_paths(json_mode: bool) -> Result<()> {
    let user_config_path = get_user_config_path();
    let legacy_user_path = get_legacy_user_config_path();
    let project_path = discover_beads_dir(None)
        .ok()
        .map(|dir| dir.join("config.yaml"));

    if json_mode {
        let output = json!({
            "user_config": user_config_path.map(|p| p.display().to_string()),
            "legacy_user_config": legacy_user_path.map(|p| p.display().to_string()),
            "project_config": project_path.map(|p| p.display().to_string()),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Configuration paths:");
        println!();
        if let Some(path) = user_config_path {
            let exists = path.exists();
            let status = if exists { "(exists)" } else { "(not found)" };
            println!("  User config:    {} {}", path.display(), status);
        } else {
            println!("  User config:    (HOME not set)");
        }
        if let Some(path) = legacy_user_path {
            if path.exists() {
                println!("  Legacy user:    {} (exists)", path.display());
            }
        }
        if let Some(path) = project_path {
            let exists = path.exists();
            let status = if exists { "(exists)" } else { "(not found)" };
            println!("  Project config: {} {}", path.display(), status);
        } else {
            println!("  Project config: (no .beads directory found)");
        }
    }

    Ok(())
}

/// Open user config in editor.
fn edit_config() -> Result<()> {
    let config_path = get_user_config_path().ok_or_else(|| {
        crate::error::BeadsError::Config("HOME environment variable not set".to_string())
    })?;

    // Ensure parent directory exists
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Create file if it doesn't exist
    if !config_path.exists() {
        let default_content = r"# br configuration
# See `br config --list` for available options

# Issue ID prefix
# issue_prefix: bd

# Default priority for new issues (0-4)
# default_priority: 2

# Default issue type
# default_type: task
";
        fs::write(&config_path, default_content)?;
    }

    // Get editor
    let editor = env::var("EDITOR")
        .or_else(|_| env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());

    // Open editor
    let status = Command::new(&editor).arg(&config_path).status()?;

    if !status.success() {
        eprintln!("Editor exited with status: {status}");
    }

    Ok(())
}

/// List all available config options.
fn list_options(json_mode: bool) -> Result<()> {
    let options = get_config_options();

    if json_mode {
        let output: Vec<_> = options
            .iter()
            .map(|(key, desc, default)| {
                json!({
                    "key": key,
                    "description": desc,
                    "default": default,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Available configuration options:");
        println!();
        for (key, description, default) in options {
            println!("  {key}");
            println!("    {description}");
            if let Some(def) = default {
                println!("    Default: {def}");
            }
            println!();
        }
    }

    Ok(())
}

/// Get a specific config value.
fn get_config_value(
    key: &str,
    beads_dir: Option<&PathBuf>,
    overrides: &CliOverrides,
    json_mode: bool,
) -> Result<()> {
    let layer = if let Some(dir) = beads_dir {
        // Try to open storage for DB config
        let storage = crate::config::open_storage(dir, overrides.db.as_ref(), overrides.lock_timeout)
            .ok()
            .map(|(s, _)| s);
        load_config(dir, storage.as_ref(), overrides)?
    } else {
        // No beads dir, just use env and user configs
        let mut layer = ConfigLayer::default();
        if let Ok(user) = load_user_config() {
            layer.merge_from(&user);
        }
        layer.merge_from(&ConfigLayer::from_env());
        layer.merge_from(&overrides.as_layer());
        layer
    };

    // Look for the key in both runtime and startup
    let value = layer
        .runtime
        .get(key)
        .or_else(|| layer.startup.get(key))
        .cloned();

    if json_mode {
        let output = json!({
            "key": key,
            "value": value,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if let Some(v) = value {
        println!("{v}");
    } else {
        eprintln!("Config key not found: {key}");
        std::process::exit(1);
    }

    Ok(())
}

/// Set a config value in user config.
fn set_config_value(kv: &str, json_mode: bool) -> Result<()> {
    let (key, value) = kv.split_once('=').ok_or_else(|| {
        crate::error::BeadsError::Validation {
            field: "config".to_string(),
            reason: "Invalid format. Use: --set key=value".to_string(),
        }
    })?;

    let config_path = get_user_config_path().ok_or_else(|| {
        crate::error::BeadsError::Config("HOME environment variable not set".to_string())
    })?;

    // Ensure parent directory exists
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Load existing config or create new
    let mut config: serde_yaml::Value = if config_path.exists() {
        let contents = fs::read_to_string(&config_path)?;
        serde_yaml::from_str(&contents).unwrap_or(serde_yaml::Value::Mapping(Default::default()))
    } else {
        serde_yaml::Value::Mapping(Default::default())
    };

    // Set the value
    if let serde_yaml::Value::Mapping(ref mut map) = config {
        // Handle nested keys (e.g., "display.color")
        let parts: Vec<&str> = key.split('.').collect();
        if parts.len() == 1 {
            map.insert(
                serde_yaml::Value::String(key.to_string()),
                serde_yaml::Value::String(value.to_string()),
            );
        } else {
            // For nested keys, we need to navigate/create the structure
            let mut current = &mut config;
            for (i, part) in parts.iter().enumerate() {
                if i == parts.len() - 1 {
                    // Last part - set the value
                    if let serde_yaml::Value::Mapping(m) = current {
                        m.insert(
                            serde_yaml::Value::String(part.to_string()),
                            serde_yaml::Value::String(value.to_string()),
                        );
                    }
                } else {
                    // Navigate or create nested mapping
                    if let serde_yaml::Value::Mapping(m) = current {
                        let key = serde_yaml::Value::String(part.to_string());
                        if !m.contains_key(&key) {
                            m.insert(key.clone(), serde_yaml::Value::Mapping(Default::default()));
                        }
                        current = m.get_mut(&key).unwrap();
                    }
                }
            }
        }
    }

    // Write back
    let yaml_str = serde_yaml::to_string(&config)?;
    fs::write(&config_path, yaml_str)?;

    if json_mode {
        let output = json!({
            "key": key,
            "value": value,
            "path": config_path.display().to_string(),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Set {key}={value} in {}", config_path.display());
    }

    Ok(())
}

/// Show merged configuration.
fn show_config(
    beads_dir: Option<&PathBuf>,
    overrides: &CliOverrides,
    args: &ConfigArgs,
    json_mode: bool,
) -> Result<()> {
    if args.project {
        // Show only project config
        if let Some(dir) = beads_dir {
            let layer = load_project_config(dir)?;
            return output_layer(&layer, "project", json_mode);
        }
        if json_mode {
            println!("{{}}");
        } else {
            println!("No project config (no .beads directory found)");
        }
        return Ok(());
    }

    if args.user {
        // Show only user config
        let layer = load_user_config()?;
        return output_layer(&layer, "user", json_mode);
    }

    // Show merged config
    let layer = if let Some(dir) = beads_dir {
        let storage = crate::config::open_storage(dir, overrides.db.as_ref(), overrides.lock_timeout)
            .ok()
            .map(|(s, _)| s);
        load_config(dir, storage.as_ref(), overrides)?
    } else {
        let mut layer = ConfigLayer::default();
        if let Ok(user) = load_user_config() {
            layer.merge_from(&user);
        }
        layer.merge_from(&ConfigLayer::from_env());
        layer.merge_from(&overrides.as_layer());
        layer
    };

    // Compute derived values
    let id_config = id_config_from_layer(&layer);
    let actor = resolve_actor(&layer);

    if json_mode {
        let mut all_keys: BTreeMap<String, serde_json::Value> = BTreeMap::new();

        for (k, v) in &layer.runtime {
            all_keys.insert(k.clone(), json!(v));
        }
        for (k, v) in &layer.startup {
            all_keys.insert(k.clone(), json!(v));
        }

        // Add computed values
        all_keys.insert("_computed.prefix".to_string(), json!(id_config.prefix));
        all_keys.insert(
            "_computed.min_hash_length".to_string(),
            json!(id_config.min_hash_length),
        );
        all_keys.insert(
            "_computed.max_hash_length".to_string(),
            json!(id_config.max_hash_length),
        );
        all_keys.insert("_computed.actor".to_string(), json!(actor));

        println!("{}", serde_json::to_string_pretty(&all_keys)?);
    } else {
        println!("Current configuration (merged):");
        println!();

        // Group by category
        let mut runtime_keys: Vec<_> = layer.runtime.keys().collect();
        runtime_keys.sort();

        let mut startup_keys: Vec<_> = layer.startup.keys().collect();
        startup_keys.sort();

        if !runtime_keys.is_empty() {
            println!("Runtime settings:");
            for key in runtime_keys {
                if let Some(value) = layer.runtime.get(key) {
                    println!("  {key}: {value}");
                }
            }
            println!();
        }

        if !startup_keys.is_empty() {
            println!("Startup settings:");
            for key in startup_keys {
                if let Some(value) = layer.startup.get(key) {
                    println!("  {key}: {value}");
                }
            }
            println!();
        }

        println!("Computed values:");
        println!("  prefix: {}", id_config.prefix);
        println!("  min_hash_length: {}", id_config.min_hash_length);
        println!("  max_hash_length: {}", id_config.max_hash_length);
        println!("  actor: {actor}");
    }

    Ok(())
}

/// Output a single config layer.
fn output_layer(layer: &ConfigLayer, source: &str, json_mode: bool) -> Result<()> {
    if json_mode {
        let mut all_keys: BTreeMap<String, &str> = BTreeMap::new();
        for (k, v) in &layer.runtime {
            all_keys.insert(k.clone(), v);
        }
        for (k, v) in &layer.startup {
            all_keys.insert(k.clone(), v);
        }
        println!("{}", serde_json::to_string_pretty(&all_keys)?);
    } else {
        println!("{source} configuration:");
        println!();

        let mut all_keys: Vec<_> = layer
            .runtime
            .keys()
            .chain(layer.startup.keys())
            .collect();
        all_keys.sort();
        all_keys.dedup();

        if all_keys.is_empty() {
            println!("  (empty)");
        } else {
            for key in all_keys {
                let value = layer
                    .runtime
                    .get(key)
                    .or_else(|| layer.startup.get(key))
                    .unwrap();
                println!("  {key}: {value}");
            }
        }
    }

    Ok(())
}

/// Get user config path.
fn get_user_config_path() -> Option<PathBuf> {
    env::var("HOME").ok().map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("bd")
            .join("config.yaml")
    })
}

/// Get legacy user config path.
fn get_legacy_user_config_path() -> Option<PathBuf> {
    env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".beads").join("config.yaml"))
}

/// Get list of config options with descriptions.
fn get_config_options() -> Vec<(&'static str, &'static str, Option<&'static str>)> {
    vec![
        // ID generation
        (
            "issue_prefix",
            "Prefix for issue IDs (e.g., 'bd' for bd-abc123). Alias: prefix",
            Some("bd"),
        ),
        (
            "default_priority",
            "Default priority for new issues (0-4)",
            Some("2"),
        ),
        (
            "default_type",
            "Default issue type for new issues",
            Some("task"),
        ),
        (
            "min_hash_length",
            "Minimum characters in issue ID hash",
            Some("3"),
        ),
        (
            "max_hash_length",
            "Maximum characters in issue ID hash",
            Some("8"),
        ),
        (
            "max_collision_prob",
            "Maximum collision probability before extending hash",
            Some("0.25"),
        ),
        // Actor
        (
            "actor",
            "Default actor name for audit trail (falls back to $USER)",
            None,
        ),
        // Paths
        ("db", "Override database path", None),
        // Behavior flags
        (
            "no-db",
            "Operate in JSONL-only mode (no database)",
            Some("false"),
        ),
        (
            "no-auto-flush",
            "Disable automatic JSONL export after mutations",
            Some("false"),
        ),
        (
            "no-auto-import",
            "Disable automatic JSONL import check on startup",
            Some("false"),
        ),
        (
            "no-daemon",
            "Force direct mode (no daemon) - effectively no-op in br",
            Some("false"),
        ),
        (
            "lock-timeout",
            "SQLite busy timeout in milliseconds",
            Some("30000"),
        ),
        (
            "flush-debounce",
            "Auto-flush debounce interval in milliseconds",
            Some("500"),
        ),
        (
            "remote-sync-interval",
            "Remote sync interval in milliseconds (legacy bd; no-op in br v1)",
            None,
        ),
        // Git/sync (startup only)
        ("git.branch", "Git branch for sync operations", None),
        ("sync.branch", "Sync branch (alias for git.branch)", None),
        // Identity
        (
            "identity",
            "Identity string for multi-user environments",
            None,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_config_options_returns_options() {
        let options = get_config_options();
        assert!(!options.is_empty());

        // Check that issue_prefix is in the list
        let has_prefix = options.iter().any(|(k, _, _)| *k == "issue_prefix");
        assert!(has_prefix);
    }

    #[test]
    fn test_user_config_path_format() {
        // This test may fail if HOME is not set, which is fine
        if let Some(path) = get_user_config_path() {
            assert!(path.ends_with("config.yaml"));
            assert!(path.to_string_lossy().contains(".config/bd"));
        }
    }

    #[test]
    fn test_set_config_invalid_format() {
        // Test with empty HOME - will fail with proper error
        let result = set_config_value("no_equals_sign", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_nested_key_parsing() {
        // Test the key parsing logic - "display.color" should have 2 parts
        let parts: Vec<&str> = "display.color".split('.').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "display");
        assert_eq!(parts[1], "color");
    }
}
