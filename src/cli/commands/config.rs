//! Configuration management command.
//!
//! Provides CLI access to the layered configuration system:
//! - Show current merged configuration
//! - Get/set individual config values
//! - List all available options
//! - Open config in editor
//! - Show config file paths

#![allow(clippy::default_trait_access)]

use crate::cli::ConfigCommands;
use crate::config::{
    self, CliOverrides, ConfigLayer, ConfigPaths, default_config_layer, discover_beads_dir,
    id_config_from_layer, load_legacy_user_config, load_project_config, load_user_config,
    resolve_actor,
};
use crate::error::Result;
use crate::output::OutputContext;
use rich_rust::prelude::*;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{debug, info, trace};

#[derive(Debug, Clone, Copy)]
enum ConfigSource {
    Default,
    Db,
    LegacyUser,
    User,
    Project,
    Environment,
    Cli,
}

impl ConfigSource {
    fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Db => "db",
            Self::LegacyUser => "legacy user",
            Self::User => "user config",
            Self::Project => ".beads/config",
            Self::Environment => "environment",
            Self::Cli => "cli",
        }
    }

    fn heading(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Db => "DB",
            Self::LegacyUser => "Legacy User",
            Self::User => "User",
            Self::Project => "Project",
            Self::Environment => "Environment",
            Self::Cli => "CLI",
        }
    }
}

struct ConfigEntry {
    key: String,
    value: String,
    source: ConfigSource,
}

struct LayerWithSource {
    source: ConfigSource,
    layer: ConfigLayer,
}

/// Execute the config command.
///
/// # Errors
///
/// Returns an error if config cannot be loaded or operations fail.
pub fn execute(
    command: &ConfigCommands,
    json_mode: bool,
    overrides: &CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    match command {
        ConfigCommands::Path => show_paths(json_mode, ctx),
        ConfigCommands::Edit => edit_config(),
        ConfigCommands::List { project, user } => {
            let beads_dir = discover_beads_dir(None).ok();
            show_config(
                beads_dir.as_ref(),
                overrides,
                *project,
                *user,
                json_mode,
                ctx,
            )
        }
        ConfigCommands::Set { args } => set_config_value(args, json_mode, ctx),
        ConfigCommands::Delete { key } => delete_config_value(key, json_mode, overrides, ctx),
        ConfigCommands::Get { key } => {
            let beads_dir = discover_beads_dir(None).ok();
            get_config_value(key, beads_dir.as_ref(), overrides, json_mode, ctx)
        }
    }
}

fn build_layers(
    beads_dir: Option<&PathBuf>,
    overrides: &CliOverrides,
) -> Result<Vec<LayerWithSource>> {
    let defaults = default_config_layer();

    let db_layer = if let Some(dir) = beads_dir {
        let storage = config::open_storage_with_cli(dir, overrides)
            .ok()
            .map(|ctx| ctx.storage);
        if let Some(storage) = storage {
            ConfigLayer::from_db(&storage)?
        } else {
            ConfigLayer::default()
        }
    } else {
        ConfigLayer::default()
    };

    let legacy_user = load_legacy_user_config()?;
    let user = load_user_config()?;
    let project = if let Some(dir) = beads_dir {
        load_project_config(dir)?
    } else {
        ConfigLayer::default()
    };
    let env_layer = ConfigLayer::from_env();
    let cli_layer = overrides.as_layer();

    Ok(vec![
        LayerWithSource {
            source: ConfigSource::Default,
            layer: defaults,
        },
        LayerWithSource {
            source: ConfigSource::Db,
            layer: db_layer,
        },
        LayerWithSource {
            source: ConfigSource::LegacyUser,
            layer: legacy_user,
        },
        LayerWithSource {
            source: ConfigSource::User,
            layer: user,
        },
        LayerWithSource {
            source: ConfigSource::Project,
            layer: project,
        },
        LayerWithSource {
            source: ConfigSource::Environment,
            layer: env_layer,
        },
        LayerWithSource {
            source: ConfigSource::Cli,
            layer: cli_layer,
        },
    ])
}

fn merge_layers(layers: &[LayerWithSource]) -> ConfigLayer {
    let mut merged = ConfigLayer::default();
    for layer in layers {
        merged.merge_from(&layer.layer);
    }
    merged
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueTransform {
    Identity,
    InvertBool,
}

fn normalize_config_key(key: &str) -> String {
    key.trim().to_lowercase().replace('_', "-")
}

fn canonical_config_key(key: &str) -> (String, ValueTransform) {
    match normalize_config_key(key).as_str() {
        "issue-prefix" | "id.prefix" | "id-prefix" | "prefix" => {
            ("issue_prefix".to_string(), ValueTransform::Identity)
        }
        "default-priority" | "defaults.priority" | "defaults-priority" => {
            ("default_priority".to_string(), ValueTransform::Identity)
        }
        "default-type" | "defaults.type" | "defaults-type" => {
            ("default_type".to_string(), ValueTransform::Identity)
        }
        "display-color" | "output.color" | "output-color" => {
            ("display.color".to_string(), ValueTransform::Identity)
        }
        "no-auto-flush" | "no.auto.flush" => {
            ("sync.auto_flush".to_string(), ValueTransform::InvertBool)
        }
        "no-auto-import" | "no.auto.import" => {
            ("sync.auto_import".to_string(), ValueTransform::InvertBool)
        }
        "sync.auto-flush" | "sync.auto.flush" | "sync-auto-flush" => {
            ("sync.auto_flush".to_string(), ValueTransform::Identity)
        }
        "sync.auto-import" | "sync.auto.import" | "sync-auto-import" => {
            ("sync.auto_import".to_string(), ValueTransform::Identity)
        }
        _ => (key.to_string(), ValueTransform::Identity),
    }
}

fn parse_bool_like(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn apply_transform_to_value(value: &str, transform: ValueTransform) -> String {
    match transform {
        ValueTransform::Identity => value.to_string(),
        ValueTransform::InvertBool => parse_bool_like(value)
            .map_or_else(|| value.to_string(), |bool_value| (!bool_value).to_string()),
    }
}

fn delete_key_candidates(raw_key: &str) -> Vec<String> {
    let (canonical_key, _) = canonical_config_key(raw_key);
    let mut keys = vec![canonical_key.clone()];

    match canonical_key.as_str() {
        "sync.auto_flush" => {
            keys.extend([
                "sync.auto-flush".to_string(),
                "sync.auto.flush".to_string(),
                "sync-auto-flush".to_string(),
                "no-auto-flush".to_string(),
                "no_auto_flush".to_string(),
                "no.auto.flush".to_string(),
            ]);
        }
        "sync.auto_import" => {
            keys.extend([
                "sync.auto-import".to_string(),
                "sync.auto.import".to_string(),
                "sync-auto-import".to_string(),
                "no-auto-import".to_string(),
                "no_auto_import".to_string(),
                "no.auto.import".to_string(),
            ]);
        }
        "issue_prefix" => {
            keys.extend([
                "issue-prefix".to_string(),
                "id.prefix".to_string(),
                "id-prefix".to_string(),
                "prefix".to_string(),
            ]);
        }
        "default_priority" => {
            keys.extend([
                "default-priority".to_string(),
                "defaults.priority".to_string(),
                "defaults-priority".to_string(),
            ]);
        }
        "default_type" => {
            keys.extend([
                "default-type".to_string(),
                "defaults.type".to_string(),
                "defaults-type".to_string(),
            ]);
        }
        "display.color" => {
            keys.extend([
                "display-color".to_string(),
                "output.color".to_string(),
                "output-color".to_string(),
            ]);
        }
        _ => {}
    }

    keys.sort();
    keys.dedup();
    keys
}

fn map_contains_key(map: &HashMap<String, String>, key: &str) -> bool {
    let target = normalize_config_key(key);
    map.keys()
        .any(|candidate| normalize_config_key(candidate) == target)
}

fn map_get_value(map: &HashMap<String, String>, key: &str) -> Option<String> {
    let target = normalize_config_key(key);
    map.iter().find_map(|(candidate, value)| {
        (normalize_config_key(candidate) == target).then(|| value.clone())
    })
}

fn get_layer_value(layer: &ConfigLayer, key: &str) -> Option<String> {
    map_get_value(&layer.runtime, key).or_else(|| map_get_value(&layer.startup, key))
}

fn resolve_source(key: &str, layers: &[LayerWithSource]) -> ConfigSource {
    for layer in layers.iter().rev() {
        if map_contains_key(&layer.layer.runtime, key)
            || map_contains_key(&layer.layer.startup, key)
        {
            return layer.source;
        }
    }
    ConfigSource::Default
}

fn format_config_value(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "\"\"".to_string();
    }
    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        return trimmed.to_string();
    }
    if trimmed.parse::<i64>().is_ok() || trimmed.parse::<f64>().is_ok() {
        return trimmed.to_string();
    }
    let lower = trimmed.to_ascii_lowercase();
    if matches!(lower.as_str(), "true" | "false" | "null") {
        return trimmed.to_string();
    }
    format!("\"{trimmed}\"")
}

fn render_config_table(title: &str, entries: &[ConfigEntry], ctx: &OutputContext) {
    let theme = ctx.theme();
    if entries.is_empty() {
        let panel = Panel::from_text("No configuration values found.")
            .title(Text::styled(title, theme.panel_title.clone()))
            .box_style(theme.box_style)
            .border_style(theme.panel_border.clone());
        ctx.render(&panel);
        return;
    }

    let mut table = Table::new()
        .box_style(theme.box_style)
        .border_style(theme.panel_border.clone())
        .title(Text::styled(title, theme.panel_title.clone()));

    table = table
        .with_column(Column::new("Key").min_width(16).max_width(30))
        .with_column(Column::new("Value").min_width(12).max_width(50))
        .with_column(Column::new("Source").min_width(12).max_width(20));

    for entry in entries {
        let key_cell = Cell::new(Text::styled(&entry.key, theme.emphasis.clone()));
        let value_cell = Cell::new(Text::new(entry.value.clone()));
        let source_cell = Cell::new(Text::styled(entry.source.label(), theme.dimmed.clone()));
        table.add_row(Row::new(vec![key_cell, value_cell, source_cell]));
    }

    ctx.render(&table);
}

fn render_kv_table(title: &str, rows: &[(String, String)], ctx: &OutputContext) {
    let theme = ctx.theme();
    if rows.is_empty() {
        return;
    }
    let mut table = Table::new()
        .box_style(theme.box_style)
        .border_style(theme.panel_border.clone())
        .title(Text::styled(title, theme.panel_title.clone()));

    table = table
        .with_column(Column::new("Key").min_width(16).max_width(30))
        .with_column(Column::new("Value").min_width(12).max_width(50));

    for (key, value) in rows {
        let key_cell = Cell::new(Text::styled(key, theme.emphasis.clone()));
        let value_cell = Cell::new(Text::new(value.clone()));
        table.add_row(Row::new(vec![key_cell, value_cell]));
    }

    ctx.render(&table);
}
/// Show config file paths.
fn show_paths(_json_mode: bool, ctx: &OutputContext) -> Result<()> {
    let beads_dir = discover_beads_dir(Some(Path::new(".")))?;
    let paths = ConfigPaths::resolve(&beads_dir, None)?;
    let user_config_path = get_user_config_path();
    let legacy_user_path = get_legacy_user_config_path();
    let project_path = Some(paths.beads_dir.join("config.yaml"));

    if ctx.is_json() {
        let output = json!({
            "user_config": user_config_path.map(|p| p.display().to_string()),
            "legacy_user_config": legacy_user_path.map(|p| p.display().to_string()),
            "project_config": project_path.map(|p| p.display().to_string()),
        });
        ctx.json_pretty(&output);
    } else {
        if let Some(path) = user_config_path {
            let exists = path.exists();
            let status = if exists { "exists" } else { "not found" };
            println!("User config: {} ({})", path.display(), status);
        } else {
            println!("User config: (none)");
        }

        if let Some(path) = legacy_user_path
            && path.exists()
        {
            println!("Legacy user config: {} (found)", path.display());
        }

        if let Some(path) = project_path {
            let exists = path.exists();
            let status = if exists { "exists" } else { "not found" };
            println!("Project config: {} ({})", path.display(), status);
        } else {
            println!("Project config: (none)");
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

/// Get a specific config value.
fn get_config_value(
    key: &str,
    beads_dir: Option<&PathBuf>,
    overrides: &CliOverrides,
    _json_mode: bool,
    ctx: &OutputContext,
) -> Result<()> {
    debug!(key, "Reading config key");
    let layers = build_layers(beads_dir, overrides)?;
    let layer = merge_layers(&layers);

    let (lookup_key, transform) = canonical_config_key(key);
    let value = get_layer_value(&layer, &lookup_key)
        .map(|resolved_value| apply_transform_to_value(&resolved_value, transform));

    if ctx.is_json() {
        let output = json!({
            "key": key,
            "value": value,
            "requested_key": key,
            "resolved_key": lookup_key,
        });
        ctx.json_pretty(&output);
    } else if let Some(v) = value {
        if ctx.is_quiet() {
            return Ok(());
        }
        if ctx.is_rich() {
            let source = resolve_source(&lookup_key, &layers);
            trace!(key, source = ?source, "Config source resolved");
            render_config_table(
                "Config Value",
                &[ConfigEntry {
                    key: key.to_string(),
                    value: format_config_value(&v),
                    source,
                }],
                ctx,
            );
        } else {
            println!("{v}");
        }
    } else {
        eprintln!("Config key not found: {key}");
        std::process::exit(1);
    }

    Ok(())
}

/// Set a config value in project config (if available) or user config.
fn set_config_value(args: &[String], _json_mode: bool, ctx: &OutputContext) -> Result<()> {
    let (raw_key, raw_value) = match args.len() {
        1 => args[0]
            .split_once('=')
            .ok_or_else(|| crate::error::BeadsError::Validation {
                field: "config".to_string(),
                reason: "Invalid format. Use: --set key=value or --set key value".to_string(),
            })?,
        2 => (args[0].as_str(), args[1].as_str()),
        _ => {
            return Err(crate::error::BeadsError::Validation {
                field: "config".to_string(),
                reason: "Invalid number of arguments".to_string(),
            });
        }
    };

    let (key, transform) = canonical_config_key(raw_key);
    let value = apply_transform_to_value(raw_value, transform);

    // Determine target config file
    let (config_path, is_project) = if let Ok(beads_dir) = discover_beads_dir(None) {
        (beads_dir.join("config.yaml"), true)
    } else {
        let path = get_user_config_path().ok_or_else(|| {
            crate::error::BeadsError::Config("HOME environment variable not set".to_string())
        })?;
        (path, false)
    };

    // Ensure parent directory exists
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Load existing config or create new
    // Note: Files with only YAML comments parse as Null, not as an error
    let mut config: serde_yml::Value = if config_path.exists() {
        let contents = fs::read_to_string(&config_path)?;
        match serde_yml::from_str(&contents) {
            Ok(serde_yml::Value::Null) | Err(_) => {
                serde_yml::Value::Mapping(serde_yml::Mapping::default())
            }
            Ok(v) => v,
        }
    } else {
        serde_yml::Value::Mapping(serde_yml::Mapping::default())
    };

    // Set the value
    let parts: Vec<&str> = key.split('.').collect();
    let old_value = get_yaml_value(&config, &parts);
    set_yaml_value(&mut config, &parts, serde_yml::Value::String(value.clone()));

    // Write back
    let yaml_str = serde_yml::to_string(&config)?;
    fs::write(&config_path, yaml_str)?;

    info!(
        key,
        old_value = old_value.as_deref(),
        new_value = %value,
        "Config updated"
    );

    if ctx.is_json() {
        let output = json!({
            "key": key,
            "value": value,
            "path": config_path.display().to_string(),
            "scope": if is_project { "project" } else { "user" }
        });
        ctx.json_pretty(&output);
    } else if ctx.is_quiet() {
        return Ok(());
    } else if ctx.is_rich() {
        let theme = ctx.theme();
        let mut content = Text::new("");
        content.append_styled("Configuration updated\n", theme.emphasis.clone());
        content.append("\n");

        content.append_styled("Key: ", theme.dimmed.clone());
        content.append_styled(&key, theme.issue_title.clone());
        content.append("\n");

        content.append_styled("Value: ", theme.dimmed.clone());
        content.append(&format_config_value(&value));
        content.append("\n");

        if let Some(old) = old_value {
            content.append_styled("Previous: ", theme.dimmed.clone());
            content.append(&format_config_value(&old));
            content.append("\n");
        }

        content.append_styled("Scope: ", theme.dimmed.clone());
        content.append(if is_project { "project" } else { "user" });
        content.append("\n");

        content.append_styled("Path: ", theme.dimmed.clone());
        content.append(&config_path.display().to_string());
        content.append("\n");

        let panel = Panel::from_rich_text(&content, ctx.width())
            .title(Text::styled("Config Set", theme.panel_title.clone()))
            .box_style(theme.box_style)
            .border_style(theme.panel_border.clone());

        ctx.render(&panel);
    } else {
        println!("Set {key}={value} in {}", config_path.display());
    }

    Ok(())
}

fn set_yaml_value(config: &mut serde_yml::Value, parts: &[&str], value: serde_yml::Value) {
    if parts.is_empty() {
        return;
    }

    if !matches!(config, serde_yml::Value::Mapping(_)) {
        *config = serde_yml::Value::Mapping(serde_yml::Mapping::default());
    }

    if parts.len() == 1 {
        if let serde_yml::Value::Mapping(map) = config {
            map.insert(serde_yml::Value::String(parts[0].to_string()), value);
        }
        return;
    }

    if let serde_yml::Value::Mapping(map) = config {
        let key = serde_yml::Value::String(parts[0].to_string());
        let entry = map
            .entry(key)
            .or_insert_with(|| serde_yml::Value::Mapping(serde_yml::Mapping::default()));

        if !matches!(entry, serde_yml::Value::Mapping(_)) {
            *entry = serde_yml::Value::Mapping(serde_yml::Mapping::default());
        }

        set_yaml_value(entry, &parts[1..], value);
    }
}

fn get_yaml_value(value: &serde_yml::Value, parts: &[&str]) -> Option<String> {
    if parts.is_empty() {
        return None;
    }

    if let serde_yml::Value::Mapping(map) = value {
        let key = serde_yml::Value::String(parts[0].to_string());
        let child = map.get(&key)?;
        if parts.len() == 1 {
            return yaml_value_to_string(child);
        }
        return get_yaml_value(child, &parts[1..]);
    }

    None
}

fn yaml_value_to_string(value: &serde_yml::Value) -> Option<String> {
    match value {
        serde_yml::Value::Null => Some("null".to_string()),
        serde_yml::Value::Bool(value) => Some(value.to_string()),
        serde_yml::Value::Number(value) => Some(value.to_string()),
        serde_yml::Value::String(value) => Some(value.clone()),
        _ => serde_yml::to_string(value)
            .ok()
            .map(|value| value.trim().to_string()),
    }
}

/// Delete a config value from the database, project config, and user config.
#[allow(clippy::too_many_lines)]
fn delete_config_value(
    key: &str,
    _json_mode: bool,
    overrides: &CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let (canonical_key, _) = canonical_config_key(key);
    let delete_keys = delete_key_candidates(key);

    // 1. Delete from DB
    let beads_dir = discover_beads_dir(None).ok();
    let mut db_deleted = false;

    if let Some(dir) = &beads_dir {
        // Only try to open DB if we have a beads dir
        if let Ok(mut storage_ctx) = config::open_storage_with_cli(dir, overrides) {
            // We ignore is_startup_key check here to allow deleting from YAML even if not in DB
            match storage_ctx.storage.delete_config(&canonical_key) {
                Ok(deleted) => db_deleted = deleted,
                Err(e) => {
                    tracing::warn!(key = %canonical_key, error = %e, "Failed to delete config key from DB");
                    eprintln!("Warning: failed to delete '{canonical_key}' from DB: {e}");
                }
            }
        }
    }

    // 2. Delete from Project YAML
    let mut project_deleted = false;
    if let Some(dir) = &beads_dir {
        let config_path = dir.join("config.yaml");
        if config_path.exists() {
            let contents = fs::read_to_string(&config_path)?;
            let mut config: serde_yml::Value = serde_yml::from_str(&contents)
                .unwrap_or(serde_yml::Value::Mapping(serde_yml::Mapping::default()));

            let mut deleted_any = false;
            for delete_key in &delete_keys {
                deleted_any |= delete_from_yaml(&mut config, delete_key);
            }

            if deleted_any {
                let yaml_str = serde_yml::to_string(&config)?;
                fs::write(&config_path, yaml_str)?;
                project_deleted = true;
            }
        }
    }

    // 3. Delete from User YAML
    let mut user_deleted = false;
    if let Some(config_path) = get_user_config_path()
        && config_path.exists()
    {
        let contents = fs::read_to_string(&config_path)?;
        let mut config: serde_yml::Value = serde_yml::from_str(&contents)
            .unwrap_or(serde_yml::Value::Mapping(serde_yml::Mapping::default()));

        let mut deleted_any = false;
        for delete_key in &delete_keys {
            deleted_any |= delete_from_yaml(&mut config, delete_key);
        }

        if deleted_any {
            let yaml_str = serde_yml::to_string(&config)?;
            fs::write(&config_path, yaml_str)?;
            user_deleted = true;
        }
    }

    if ctx.is_json() {
        let output = json!({
            "key": canonical_key,
            "requested_key": key,
            "deleted_from_db": db_deleted,
            "deleted_from_project": project_deleted,
            "deleted_from_user": user_deleted,
        });
        ctx.json_pretty(&output);
    } else if ctx.is_quiet() {
        return Ok(());
    } else if db_deleted || project_deleted || user_deleted {
        let mut sources = Vec::new();
        if db_deleted {
            sources.push("DB");
        }
        if project_deleted {
            sources.push("Project");
        }
        if user_deleted {
            sources.push("User");
        }
        if ctx.is_rich() {
            let theme = ctx.theme();
            let mut content = Text::new("");
            content.append_styled("Configuration deleted\n", theme.emphasis.clone());
            content.append("\n");
            content.append_styled("Key: ", theme.dimmed.clone());
            content.append_styled(&canonical_key, theme.issue_title.clone());
            content.append("\n");
            content.append_styled("Sources: ", theme.dimmed.clone());
            content.append(&sources.join(", "));
            content.append("\n");

            let panel = Panel::from_rich_text(&content, ctx.width())
                .title(Text::styled("Config Delete", theme.panel_title.clone()))
                .box_style(theme.box_style)
                .border_style(theme.panel_border.clone());
            ctx.render(&panel);
        } else {
            println!(
                "Deleted config key: {canonical_key} (from {})",
                sources.join(", ")
            );
        }
    } else if ctx.is_rich() {
        let theme = ctx.theme();
        let message = format!("Config key not found: {canonical_key}");
        let panel = Panel::from_text(&message)
            .title(Text::styled("Config Delete", theme.panel_title.clone()))
            .box_style(theme.box_style)
            .border_style(theme.panel_border.clone());
        ctx.render(&panel);
    } else {
        println!("Config key not found: {canonical_key}");
    }

    Ok(())
}
fn delete_from_yaml(value: &mut serde_yml::Value, key: &str) -> bool {
    let parts: Vec<&str> = key.split('.').collect();
    delete_nested(value, &parts)
}

fn delete_nested(value: &mut serde_yml::Value, path: &[&str]) -> bool {
    if path.is_empty() {
        return false;
    }

    if let serde_yml::Value::Mapping(map) = value {
        let key = serde_yml::Value::String(path[0].to_string());

        if path.len() == 1 {
            return map.remove(&key).is_some();
        }

        if let Some(child) = map.get_mut(&key) {
            return delete_nested(child, &path[1..]);
        }
    }
    false
}

/// Show merged configuration.
#[allow(clippy::too_many_lines)]
fn show_config(
    beads_dir: Option<&PathBuf>,
    overrides: &CliOverrides,
    project_only: bool,
    user_only: bool,
    json_mode: bool,
    ctx: &OutputContext,
) -> Result<()> {
    if project_only {
        // Show only project config
        if let Some(dir) = beads_dir {
            let layer = load_project_config(dir)?;
            output_layer(&layer, ConfigSource::Project, json_mode, ctx);
            return Ok(());
        }
        if ctx.is_json() {
            ctx.json(&serde_json::Map::new());
        } else if ctx.is_quiet() {
            return Ok(());
        } else if ctx.is_rich() {
            let theme = ctx.theme();
            let panel = Panel::from_text("No project config (no .beads directory found).")
                .title(Text::styled(
                    "Project Configuration",
                    theme.panel_title.clone(),
                ))
                .box_style(theme.box_style)
                .border_style(theme.panel_border.clone());
            ctx.render(&panel);
        } else {
            println!("No project config (no .beads directory found)");
        }
        return Ok(());
    }

    if user_only {
        // Show only user config
        let layer = load_user_config()?;
        output_layer(&layer, ConfigSource::User, json_mode, ctx);
        return Ok(());
    }

    // Show merged config
    let layers = build_layers(beads_dir, overrides)?;
    let layer = merge_layers(&layers);

    // Compute derived values
    let id_config = id_config_from_layer(&layer);
    let actor = resolve_actor(&layer);

    if ctx.is_json() {
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

        ctx.json_pretty(&all_keys);
    } else if ctx.is_quiet() {
        return Ok(());
    } else if ctx.is_rich() {
        let mut entries = Vec::new();
        let mut keys: Vec<_> = layer.runtime.keys().chain(layer.startup.keys()).collect();
        keys.sort();
        keys.dedup();

        for key in keys {
            let value = layer
                .runtime
                .get(key)
                .or_else(|| layer.startup.get(key))
                .cloned()
                .unwrap_or_default();
            let source = resolve_source(key, &layers);
            trace!(key, source = ?source, "Config source resolved");
            entries.push(ConfigEntry {
                key: key.clone(),
                value: format_config_value(&value),
                source,
            });
        }

        render_config_table("Configuration", &entries, ctx);

        let computed_rows = vec![
            ("prefix".to_string(), format_config_value(&id_config.prefix)),
            (
                "min_hash_length".to_string(),
                format_config_value(&id_config.min_hash_length.to_string()),
            ),
            (
                "max_hash_length".to_string(),
                format_config_value(&id_config.max_hash_length.to_string()),
            ),
            ("actor".to_string(), format_config_value(&actor)),
        ];
        render_kv_table("Computed Values", &computed_rows, ctx);
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
fn output_layer(layer: &ConfigLayer, source: ConfigSource, _json_mode: bool, ctx: &OutputContext) {
    if ctx.is_json() {
        let mut all_keys: BTreeMap<String, &str> = BTreeMap::new();
        for (k, v) in &layer.runtime {
            all_keys.insert(k.clone(), v);
        }
        for (k, v) in &layer.startup {
            all_keys.insert(k.clone(), v);
        }
        ctx.json_pretty(&all_keys);
    } else if ctx.is_quiet() {
        // Nothing to output in quiet mode
    } else if ctx.is_rich() {
        let mut all_keys: Vec<_> = layer.runtime.keys().chain(layer.startup.keys()).collect();
        all_keys.sort();
        all_keys.dedup();

        let entries = all_keys
            .into_iter()
            .filter_map(|key| {
                let value = layer
                    .runtime
                    .get(key)
                    .or_else(|| layer.startup.get(key))
                    .cloned()?;
                Some(ConfigEntry {
                    key: key.clone(),
                    value: format_config_value(&value),
                    source,
                })
            })
            .collect::<Vec<_>>();

        render_config_table(
            &format!("{} Configuration", source.heading()),
            &entries,
            ctx,
        );
    } else {
        println!("{} configuration:", source.heading());
        println!();

        let mut all_keys: Vec<_> = layer.runtime.keys().chain(layer.startup.keys()).collect();
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
}

/// Get user config path.
fn get_user_config_path() -> Option<PathBuf> {
    let home = env::var("HOME").ok()?;
    let config_root = PathBuf::from(home).join(".config");
    let beads_path = config_root.join("beads").join("config.yaml");
    if beads_path.exists() {
        return Some(beads_path);
    }
    let legacy_path = config_root.join("bd").join("config.yaml");
    if legacy_path.exists() {
        return Some(legacy_path);
    }
    Some(beads_path)
}

/// Get legacy user config path.
fn get_legacy_user_config_path() -> Option<PathBuf> {
    env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".beads").join("config.yaml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_config_path_format() {
        // This test may fail if HOME is not set, which is fine
        if let Some(path) = get_user_config_path() {
            assert!(path.ends_with("config.yaml"));
            let path_str = path.to_string_lossy();
            assert!(
                path_str.contains(".config/beads") || path_str.contains(".config/bd"),
                "unexpected user config path: {path_str}"
            );
        }
    }

    #[test]
    fn test_set_config_invalid_format() {
        // Test with empty HOME - will fail with proper error
        let args = vec!["no_equals_sign".to_string()];
        let ctx = OutputContext::from_flags(false, false, true);
        let result = set_config_value(&args, false, &ctx);
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

    #[test]
    fn test_canonical_config_key_aliases() {
        assert_eq!(
            canonical_config_key("id.prefix"),
            ("issue_prefix".to_string(), ValueTransform::Identity)
        );
        assert_eq!(
            canonical_config_key("defaults.priority"),
            ("default_priority".to_string(), ValueTransform::Identity)
        );
        assert_eq!(
            canonical_config_key("defaults.type"),
            ("default_type".to_string(), ValueTransform::Identity)
        );
        assert_eq!(
            canonical_config_key("output.color"),
            ("display.color".to_string(), ValueTransform::Identity)
        );
        assert_eq!(
            canonical_config_key("no-auto-flush"),
            ("sync.auto_flush".to_string(), ValueTransform::InvertBool)
        );
        assert_eq!(
            canonical_config_key("sync.auto.flush"),
            ("sync.auto_flush".to_string(), ValueTransform::Identity)
        );
        assert_eq!(
            canonical_config_key("no_auto_import"),
            ("sync.auto_import".to_string(), ValueTransform::InvertBool)
        );
    }

    #[test]
    fn test_apply_transform_to_value_inverts_bools() {
        assert_eq!(
            apply_transform_to_value("true", ValueTransform::InvertBool),
            "false"
        );
        assert_eq!(
            apply_transform_to_value("false", ValueTransform::InvertBool),
            "true"
        );
        assert_eq!(
            apply_transform_to_value("hello", ValueTransform::InvertBool),
            "hello"
        );
        assert_eq!(
            apply_transform_to_value("1", ValueTransform::InvertBool),
            "false"
        );
        assert_eq!(
            apply_transform_to_value("0", ValueTransform::InvertBool),
            "true"
        );
    }

    #[test]
    fn test_delete_key_candidates_include_aliases() {
        let keys = delete_key_candidates("id.prefix");
        assert!(keys.contains(&"issue_prefix".to_string()));
        assert!(keys.contains(&"id.prefix".to_string()));
        assert!(keys.contains(&"prefix".to_string()));

        let keys = delete_key_candidates("no-auto-flush");
        assert!(keys.contains(&"sync.auto_flush".to_string()));
        assert!(keys.contains(&"no-auto-flush".to_string()));
    }

    #[test]
    fn test_delete_alias_variants_removes_all_matching_keys() {
        let mut config: serde_yml::Value = serde_yml::from_str(
            r"
issue_prefix: canonical
id:
  prefix: legacy
",
        )
        .expect("parse yaml");

        let mut deleted_any = false;
        for delete_key in delete_key_candidates("id.prefix") {
            deleted_any |= delete_from_yaml(&mut config, &delete_key);
        }

        assert!(deleted_any);
        assert_eq!(get_yaml_value(&config, &["issue_prefix"]), None);
        assert_eq!(get_yaml_value(&config, &["id", "prefix"]), None);
    }

    #[test]
    fn test_set_yaml_value_overwrites_scalar_root() {
        let mut config = serde_yml::Value::String("legacy".to_string());
        let parts = ["display"];
        set_yaml_value(
            &mut config,
            &parts,
            serde_yml::Value::String("true".to_string()),
        );

        let serde_yml::Value::Mapping(map) = config else {
            unreachable!("expected mapping root");
        };
        let key = serde_yml::Value::String("display".to_string());
        assert_eq!(
            map.get(&key),
            Some(&serde_yml::Value::String("true".to_string()))
        );
    }

    #[test]
    fn test_set_yaml_value_overwrites_scalar_child() {
        let mut map = serde_yml::Mapping::default();
        map.insert(
            serde_yml::Value::String("display".to_string()),
            serde_yml::Value::String("legacy".to_string()),
        );
        let mut config = serde_yml::Value::Mapping(map);
        let parts = ["display", "color"];
        set_yaml_value(
            &mut config,
            &parts,
            serde_yml::Value::String("blue".to_string()),
        );

        let serde_yml::Value::Mapping(root) = config else {
            unreachable!("expected mapping root");
        };
        let display_key = serde_yml::Value::String("display".to_string());
        let Some(serde_yml::Value::Mapping(display_map)) = root.get(&display_key) else {
            unreachable!("expected display mapping");
        };
        let color_key = serde_yml::Value::String("color".to_string());
        assert_eq!(
            display_map.get(&color_key),
            Some(&serde_yml::Value::String("blue".to_string()))
        );
    }
}
