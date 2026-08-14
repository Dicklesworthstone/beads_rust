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
    self, CliOverrides, ConfigLayer, ConfigPaths, default_config_layer,
    discover_optional_beads_dir_with_cli, id_config_from_layer, load_legacy_user_config,
    load_project_config, load_user_config, resolve_actor,
};
use crate::error::{BeadsError, Result};
use crate::output::OutputContext;
use crate::util::id::normalize_configured_prefix;
use crate::franken_sync::Connection;
use fsqlite_types::SqliteValue;
use rich_rust::prelude::*;
use serde_json::json;
use shell_words::split as split_shell_words;
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{debug, info, trace};

#[derive(Debug, Clone, Copy)]
enum ConfigSource {
    Default,
    Jsonl,
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
            Self::Jsonl => "jsonl inference",
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
            Self::Jsonl => "JSONL",
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

struct TempConfigFileGuard {
    path: PathBuf,
    persist: bool,
}

impl TempConfigFileGuard {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            persist: false,
        }
    }

    fn persist(&mut self) {
        self.persist = true;
    }
}

impl Drop for TempConfigFileGuard {
    fn drop(&mut self) {
        if !self.persist {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn create_temp_config_file(config_path: &Path) -> Result<(PathBuf, File)> {
    let pid = std::process::id();
    for attempt in 0..64_u32 {
        let extension = if attempt == 0 {
            format!("yaml.{pid}.tmp")
        } else {
            format!("yaml.{pid}.{attempt}.tmp")
        };
        let temp_path = config_path.with_extension(extension);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }

    Err(BeadsError::Config(format!(
        "Failed to allocate temp config file for {}",
        config_path.display()
    )))
}

fn write_config_atomically(config_path: &Path, yaml: &str) -> Result<()> {
    let existing_permissions = match fs::symlink_metadata(config_path) {
        Ok(metadata) => {
            validate_edit_config_target(config_path, &metadata)?;
            Some(metadata.permissions())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let (temp_path, mut temp_file) = create_temp_config_file(config_path)?;
    let mut guard = TempConfigFileGuard::new(temp_path.clone());
    if let Some(permissions) = existing_permissions
        && let Err(error) = fs::set_permissions(&temp_path, permissions)
    {
        tracing::warn!(
            path = %config_path.display(),
            error = %error,
            "Failed to apply original config file permissions before atomic rewrite"
        );
    }
    temp_file.write_all(yaml.as_bytes())?;
    temp_file.sync_all()?;
    drop(temp_file);
    crate::util::durable_rename(&temp_path, config_path)?;
    guard.persist();
    Ok(())
}

fn validate_edit_config_target(config_path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() {
        return Err(BeadsError::Config(format!(
            "Refusing to edit config through symbolic link: {}",
            config_path.display()
        )));
    }
    if !metadata.is_file() {
        return Err(BeadsError::Config(format!(
            "Config path is not a regular file: {}",
            config_path.display()
        )));
    }
    Ok(())
}

fn create_default_config_if_missing(config_path: &Path, default_content: &str) -> Result<()> {
    match fs::symlink_metadata(config_path) {
        Ok(metadata) => return validate_edit_config_target(config_path, &metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(config_path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(config_path)?;
            return validate_edit_config_target(config_path, &metadata);
        }
        Err(error) => return Err(error.into()),
    };
    file.write_all(default_content.as_bytes())?;
    file.sync_all()?;
    drop(file);
    crate::util::sync_parent_directory(config_path)?;
    Ok(())
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
        ConfigCommands::Path => show_paths(json_mode, overrides, ctx),
        ConfigCommands::Edit => edit_config(),
        ConfigCommands::List { project, user } => {
            let beads_dir = discover_optional_beads_dir_with_cli(overrides)?;
            show_config(
                beads_dir.as_ref(),
                overrides,
                *project,
                *user,
                json_mode,
                ctx,
            )
        }
        ConfigCommands::Set { args } => set_config_value(args, json_mode, overrides, ctx),
        ConfigCommands::Delete { key } => delete_config_value(key, json_mode, overrides, ctx),
        ConfigCommands::Get { key } => {
            let beads_dir = discover_optional_beads_dir_with_cli(overrides)?;
            get_config_value(key, beads_dir.as_ref(), overrides, json_mode, ctx)
        }
    }
}

fn overrides_without_no_db(overrides: &CliOverrides) -> CliOverrides {
    let mut adjusted = overrides.clone();
    adjusted.no_db = None;
    adjusted
}

fn build_layers(
    beads_dir: Option<&PathBuf>,
    overrides: &CliOverrides,
) -> Result<Vec<LayerWithSource>> {
    let defaults = default_config_layer();
    let db_overrides = overrides_without_no_db(overrides);

    let (jsonl_inferred_layer, db_layer) = if let Some(dir) = beads_dir {
        let paths = config::resolve_paths(dir, db_overrides.db.as_ref())?;
        (
            load_jsonl_inferred_layer(&paths)?,
            load_db_layer_without_recovery(&paths),
        )
    } else {
        (ConfigLayer::default(), ConfigLayer::default())
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
            source: ConfigSource::Jsonl,
            layer: jsonl_inferred_layer,
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

fn load_jsonl_inferred_layer(paths: &ConfigPaths) -> Result<ConfigLayer> {
    let mut layer = ConfigLayer::default();
    if let Some(prefix) = config::first_prefix_from_jsonl(&paths.jsonl_path)? {
        layer.runtime.insert("issue_prefix".to_string(), prefix);
    }
    Ok(layer)
}

fn load_db_layer_without_recovery(paths: &ConfigPaths) -> ConfigLayer {
    if !paths.db_path.is_file() {
        return ConfigLayer::default();
    }

    match config::with_database_family_snapshot(&paths.db_path, |snapshot_db_path| {
        let conn = Connection::open(snapshot_db_path.to_string_lossy().into_owned())?;
        let rows = conn.query("SELECT key, value FROM config")?;
        let mut layer = ConfigLayer::default();

        for row in rows {
            let Some(key) = row.get(0).and_then(SqliteValue::as_text) else {
                continue;
            };
            let Some(value) = row.get(1).and_then(SqliteValue::as_text) else {
                continue;
            };
            if config::is_startup_key(key) {
                continue;
            }
            layer.runtime.insert(key.to_string(), value.to_string());
        }

        conn.close()?;
        Ok(layer)
    }) {
        Ok(layer) => layer,
        Err(err) => {
            debug!(
                path = %paths.db_path.display(),
                error = %err,
                "Skipping DB config layer because the database could not be snapshotted for read-only access"
            );
            ConfigLayer::default()
        }
    }
}

fn merge_layers(layers: &[LayerWithSource]) -> ConfigLayer {
    let mut merged = ConfigLayer::default();
    for layer in layers {
        merged.merge_from(&layer.layer);
    }
    merged
}

fn canonical_config_key(key: &str) -> String {
    key.trim().to_lowercase().replace('-', "_")
}

fn push_unique_config_alias(aliases: &mut Vec<String>, alias: String) {
    if !alias.is_empty() && !aliases.contains(&alias) {
        aliases.push(alias);
    }
}

fn config_key_aliases(key: &str) -> Vec<String> {
    let trimmed = key.trim();
    let mut aliases = Vec::new();
    push_unique_config_alias(&mut aliases, trimmed.to_string());

    let lower = trimmed.to_lowercase();
    push_unique_config_alias(&mut aliases, lower.clone());
    push_unique_config_alias(&mut aliases, lower.replace('-', "_"));
    push_unique_config_alias(&mut aliases, lower.replace('_', "-"));

    aliases
}

fn resolve_source(key: &str, layers: &[LayerWithSource]) -> ConfigSource {
    let canonical = canonical_config_key(key);
    for layer in layers.iter().rev() {
        if layer.layer.runtime.contains_key(&canonical)
            || layer.layer.startup.contains_key(&canonical)
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
fn show_paths(_json_mode: bool, overrides: &CliOverrides, ctx: &OutputContext) -> Result<()> {
    let paths = discover_optional_beads_dir_with_cli(overrides)?
        .map(|beads_dir| ConfigPaths::resolve(&beads_dir, overrides.db.as_ref()))
        .transpose()?;
    let user_config_path = get_user_config_path();
    let legacy_user_path = get_legacy_user_config_path();
    let project_path = paths.as_ref().and_then(ConfigPaths::project_config_path);

    if ctx.is_json() {
        let output = json!({
            "user_config": user_config_path.map(|p| p.display().to_string()),
            "legacy_user_config": legacy_user_path.map(|p| p.display().to_string()),
            "project_config": project_path.map(|p| p.display().to_string()),
        });
        ctx.json_pretty(&output);
    } else if ctx.is_quiet() {
        return Ok(());
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

    // Create the default file if needed, and reject symlinked or non-file paths
    // before passing the config path to an editor.
    let default_content = r"# br configuration
# See `br config list` for available options

# Issue ID prefix
# issue_prefix: br

# Default priority for new issues (0-4)
# default_priority: 2

# Default issue type
# default_type: task
";
    create_default_config_if_missing(&config_path, default_content)?;

    // Get editor
    let editor = env::var("EDITOR")
        .or_else(|_| env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());
    let (program, editor_args) = parse_editor_command(&editor)?;

    // Open an allowlisted editor. EDITOR/VISUAL may carry flags, but the
    // executable itself is constrained so a poisoned environment cannot turn
    // `br config edit` into a generic process launcher.
    let mut command = AllowedEditor::from_program(&program)?.command();
    let status = command.args(&editor_args).arg(&config_path).status()?;

    if !status.success() {
        eprintln!("Editor exited with status: {status}");
    }

    Ok(())
}

fn parse_editor_command(editor: &str) -> Result<(String, Vec<String>)> {
    let parts = split_shell_words(editor)
        .map_err(|e| BeadsError::Config(format!("Invalid EDITOR/VISUAL command: {e}")))?;
    let Some((program, args)) = parts.split_first() else {
        return Err(BeadsError::Config(
            "EDITOR/VISUAL command cannot be empty".to_string(),
        ));
    };
    Ok((program.clone(), args.to_vec()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AllowedEditor {
    Code,
    CodeInsiders,
    Codium,
    Cursor,
    CursorInsiders,
    Emacs,
    EmacsClient,
    Helix,
    Hx,
    Neovim,
    Neovide,
    NotepadPlusPlus,
    Mate,
    Micro,
    Nano,
    Pico,
    Notepad,
    Subl,
    True,
    Vi,
    Vim,
    Vscodium,
    Zed,
}

impl AllowedEditor {
    fn from_program(program: &str) -> Result<Self> {
        let normalized = strip_windows_exe_suffix(editor_program_name(program));

        match normalized {
            "code" => Ok(Self::Code),
            "code-insiders" => Ok(Self::CodeInsiders),
            "codium" => Ok(Self::Codium),
            "cursor" => Ok(Self::Cursor),
            "cursor-insiders" => Ok(Self::CursorInsiders),
            "emacs" => Ok(Self::Emacs),
            "emacsclient" => Ok(Self::EmacsClient),
            "helix" => Ok(Self::Helix),
            "hx" => Ok(Self::Hx),
            "nvim" => Ok(Self::Neovim),
            "neovide" => Ok(Self::Neovide),
            "mate" => Ok(Self::Mate),
            "micro" => Ok(Self::Micro),
            "nano" => Ok(Self::Nano),
            "notepad" => Ok(Self::Notepad),
            "notepad++" => Ok(Self::NotepadPlusPlus),
            "pico" => Ok(Self::Pico),
            "subl" => Ok(Self::Subl),
            "true" => Ok(Self::True),
            "vi" => Ok(Self::Vi),
            "vim" => Ok(Self::Vim),
            "vscodium" => Ok(Self::Vscodium),
            "zed" => Ok(Self::Zed),
            _ => Err(BeadsError::Config(format!(
                "Unsupported editor '{program}'. Set EDITOR or VISUAL to one of: {}",
                ALLOWED_EDITORS.join(", ")
            ))),
        }
    }

    fn command(self) -> Command {
        match self {
            Self::Code => Command::new("code"),
            Self::CodeInsiders => Command::new("code-insiders"),
            Self::Codium => Command::new("codium"),
            Self::Cursor => Command::new("cursor"),
            Self::CursorInsiders => Command::new("cursor-insiders"),
            Self::Emacs => Command::new("emacs"),
            Self::EmacsClient => Command::new("emacsclient"),
            Self::Helix => Command::new("helix"),
            Self::Hx => Command::new("hx"),
            Self::Neovim => Command::new("nvim"),
            Self::Neovide => Command::new("neovide"),
            Self::Mate => Command::new("mate"),
            Self::Micro => Command::new("micro"),
            Self::Nano => Command::new("nano"),
            Self::Notepad => Command::new("notepad"),
            Self::NotepadPlusPlus => Command::new("notepad++"),
            Self::Pico => Command::new("pico"),
            Self::Subl => Command::new("subl"),
            Self::True => Command::new("true"),
            Self::Vi => Command::new("vi"),
            Self::Vim => Command::new("vim"),
            Self::Vscodium => Command::new("vscodium"),
            Self::Zed => Command::new("zed"),
        }
    }
}

fn editor_program_name(program: &str) -> &str {
    let name = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
        .trim();
    name.rsplit(['/', '\\']).next().unwrap_or(name).trim()
}

fn strip_windows_exe_suffix(name: &str) -> &str {
    let suffix_start = name.len().saturating_sub(4);
    let Some(suffix) = name.get(suffix_start..) else {
        return name;
    };
    if suffix.eq_ignore_ascii_case(".exe") {
        name.get(..suffix_start).unwrap_or(name)
    } else {
        name
    }
}

const ALLOWED_EDITORS: &[&str] = &[
    "code",
    "code-insiders",
    "codium",
    "cursor",
    "cursor-insiders",
    "emacs",
    "emacsclient",
    "helix",
    "hx",
    "neovide",
    "nvim",
    "mate",
    "micro",
    "nano",
    "notepad",
    "notepad++",
    "pico",
    "subl",
    "true",
    "vi",
    "vim",
    "vscodium",
    "zed",
];

/// Resolve the effective DB path `br` would use at runtime for the current
/// workspace, independent of any explicit `db` config override.
///
/// Uses the same path-resolution logic (`config::resolve_paths`, honoring the
/// `--db` / `BEADS_DB` override and `.beads/metadata.json`) that the storage
/// layer uses to locate the database. Returns the canonical path (when a
/// `.beads` directory was discovered) and whether that file exists on disk.
/// Returns `(None, false)` when no workspace is discovered. #339
fn resolve_effective_db_path(
    beads_dir: Option<&PathBuf>,
    overrides: &CliOverrides,
) -> (Option<PathBuf>, bool) {
    let Some(dir) = beads_dir else {
        return (None, false);
    };
    match config::resolve_paths(dir, overrides.db.as_ref()) {
        Ok(paths) => {
            let exists = paths.db_path.exists();
            (Some(paths.db_path), exists)
        }
        Err(_) => (None, false),
    }
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
    let canonical_key = canonical_config_key(key);

    let value = layer.get(&canonical_key).map(str::to_owned);

    if ctx.is_json() {
        let mut output = json!({
            "key": key,
            "value": value,
        });
        // For the `db` key, surface the RESOLVED canonical DB path that `br`
        // actually uses at runtime (the discovered `.beads/beads.db`), even
        // when there is no explicit override (`value` is null). Robot callers
        // querying `config get db --json` need the effective path, not just
        // the raw override, plus whether that file currently exists. #339
        if canonical_key == "db" {
            let (effective_path, exists) = resolve_effective_db_path(beads_dir, overrides);
            if let Some(obj) = output.as_object_mut() {
                obj.insert(
                    "effective_path".to_string(),
                    effective_path
                        .as_ref()
                        .map_or(serde_json::Value::Null, |p| json!(p.display().to_string())),
                );
                obj.insert("exists".to_string(), json!(exists));
            }
        }
        ctx.json_pretty(&output);
    } else if let Some(v) = value {
        if ctx.is_quiet() {
            return Ok(());
        }
        if ctx.is_rich() {
            let source = resolve_source(key, &layers);
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
        return Err(crate::error::BeadsError::Config(format!(
            "Config key not found: {key}"
        )));
    }

    Ok(())
}

/// Set a config value in project config (if available) or user config.
fn set_config_value(
    args: &[String],
    _json_mode: bool,
    overrides: &CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let (key, value) = match args {
        [arg] => arg
            .split_once('=')
            .ok_or_else(|| crate::error::BeadsError::Validation {
                field: "config".to_string(),
                reason: "Invalid format. Use: --set key=value or --set key value".to_string(),
            })?,
        [key, value] => (key.as_str(), value.as_str()),
        _ => {
            return Err(crate::error::BeadsError::Validation {
                field: "config".to_string(),
                reason: "Invalid number of arguments".to_string(),
            });
        }
    };

    if canonical_config_key(key) == "issue_prefix" {
        normalize_configured_prefix(value)?;
    }

    // Determine target config file
    let (config_path, is_project) =
        if let Some(beads_dir) = discover_optional_beads_dir_with_cli(overrides)? {
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

    // Load existing config or create new.
    // Files with only YAML comments parse as Null, which we treat as an empty mapping.
    let mut config = load_mutable_yaml_config(&config_path)?;

    // Set the value
    let parts: Vec<&str> = key.split('.').collect();
    let old_value = get_yaml_value(&config, &parts);
    set_yaml_value(&mut config, &parts, parse_scalar_config_value(value));

    // Write back atomically (temp file + rename) to prevent corruption on crash
    let yaml_str = serde_yml::to_string(&config)?;
    write_config_atomically(&config_path, &yaml_str)?;

    info!(
        key,
        old_value = old_value.as_deref(),
        new_value = value,
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
        content.append_styled(key, theme.issue_title.clone());
        content.append("\n");

        content.append_styled("Value: ", theme.dimmed.clone());
        content.append(&format_config_value(value));
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

fn parse_scalar_config_value(value: &str) -> serde_yml::Value {
    value.parse::<bool>().map_or_else(
        |_| {
            value.parse::<i64>().map_or_else(
                |_| match value.parse::<f64>() {
                    Ok(parsed) => serde_yml::Value::Number(parsed.into()),
                    Err(_) if value == "null" => serde_yml::Value::Null,
                    Err(_) => serde_yml::Value::String(value.to_string()),
                },
                |parsed| serde_yml::Value::Number(parsed.into()),
            )
        },
        serde_yml::Value::Bool,
    )
}

fn set_yaml_value(config: &mut serde_yml::Value, parts: &[&str], value: serde_yml::Value) {
    let Some((part, remaining_parts)) = parts.split_first() else {
        return;
    };

    if !matches!(config, serde_yml::Value::Mapping(_)) {
        *config = serde_yml::Value::Mapping(serde_yml::Mapping::default());
    }

    if remaining_parts.is_empty() {
        if let serde_yml::Value::Mapping(map) = config {
            map.insert(serde_yml::Value::String((*part).to_string()), value);
        }
        return;
    }

    if let serde_yml::Value::Mapping(map) = config {
        let key = serde_yml::Value::String((*part).to_string());
        let entry = map
            .entry(key)
            .or_insert_with(|| serde_yml::Value::Mapping(serde_yml::Mapping::default()));

        if !matches!(entry, serde_yml::Value::Mapping(_)) {
            *entry = serde_yml::Value::Mapping(serde_yml::Mapping::default());
        }

        set_yaml_value(entry, remaining_parts, value);
    }
}

fn get_yaml_value(value: &serde_yml::Value, parts: &[&str]) -> Option<String> {
    let (part, remaining_parts) = parts.split_first()?;

    if let serde_yml::Value::Mapping(map) = value {
        let key = serde_yml::Value::String((*part).to_string());
        let child = map.get(&key)?;
        if remaining_parts.is_empty() {
            return yaml_value_to_string(child);
        }
        return get_yaml_value(child, remaining_parts);
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

fn load_mutable_yaml_config(config_path: &Path) -> Result<serde_yml::Value> {
    let metadata = match fs::symlink_metadata(config_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(serde_yml::Value::Mapping(serde_yml::Mapping::default()));
        }
        Err(error) => return Err(error.into()),
    };
    validate_edit_config_target(config_path, &metadata)?;

    let contents = fs::read_to_string(config_path)?;
    match serde_yml::from_str(&contents) {
        Ok(serde_yml::Value::Null) => Ok(serde_yml::Value::Mapping(serde_yml::Mapping::default())),
        Ok(value) => Ok(value),
        Err(err) => Err(BeadsError::Config(format!(
            "Failed to parse YAML config {}: {err}",
            config_path.display()
        ))),
    }
}

/// Delete a config value from the database, project config, and user config.
fn delete_config_value(
    key: &str,
    _json_mode: bool,
    overrides: &CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    // Pre-validate YAML targets before any mutation so malformed config files
    // do not cause a failed command after the DB value was already deleted.
    let beads_dir = discover_optional_beads_dir_with_cli(overrides)?;
    let prepared_project_delete = beads_dir
        .as_ref()
        .map(|dir| prepare_yaml_delete(&dir.join("config.yaml"), key))
        .transpose()?
        .flatten();
    let prepared_user_delete = get_user_config_path()
        .map(|path| prepare_yaml_delete(&path, key))
        .transpose()?
        .flatten();
    let prepared_legacy_user_delete = get_legacy_user_config_path()
        .map(|path| prepare_yaml_delete(&path, key))
        .transpose()?
        .flatten();

    // 1. Delete from DB
    let mut db_deleted = false;

    if let Some(dir) = &beads_dir
        && !matches!(overrides.no_db, Some(true))
    {
        let mut storage_ctx = config::open_storage_with_cli(dir, overrides)?;
        for alias in config_key_aliases(key) {
            db_deleted |= storage_ctx.storage.delete_config(&alias)?;
        }
        storage_ctx.flush_no_db_if_dirty()?;
    }

    // 2. Delete from Project YAML
    let project_deleted = apply_prepared_yaml_delete(prepared_project_delete)?;

    // 3. Delete from User YAML
    let user_deleted = apply_prepared_yaml_delete(prepared_user_delete)?;
    let legacy_user_deleted = apply_prepared_yaml_delete(prepared_legacy_user_delete)?;

    if ctx.is_json() {
        let output = json!({
            "key": key,
            "deleted_from_db": db_deleted,
            "deleted_from_project": project_deleted,
            "deleted_from_user": user_deleted || legacy_user_deleted,
        });
        ctx.json_pretty(&output);
    } else if ctx.is_quiet() {
        return Ok(());
    } else if db_deleted || project_deleted || user_deleted || legacy_user_deleted {
        let mut sources = Vec::new();
        if db_deleted {
            sources.push("DB");
        }
        if project_deleted {
            sources.push("Project");
        }
        if user_deleted || legacy_user_deleted {
            sources.push("User");
        }
        if ctx.is_rich() {
            let theme = ctx.theme();
            let mut content = Text::new("");
            content.append_styled("Configuration deleted\n", theme.emphasis.clone());
            content.append("\n");
            content.append_styled("Key: ", theme.dimmed.clone());
            content.append_styled(key, theme.issue_title.clone());
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
            println!("Deleted config key: {key} (from {})", sources.join(", "));
        }
    } else if ctx.is_rich() {
        let theme = ctx.theme();
        let message = format!("Config key not found: {key}");
        let panel = Panel::from_text(&message)
            .title(Text::styled("Config Delete", theme.panel_title.clone()))
            .box_style(theme.box_style)
            .border_style(theme.panel_border.clone());
        ctx.render(&panel);
    } else {
        println!("Config key not found: {key}");
    }

    Ok(())
}

struct PreparedYamlDelete {
    path: PathBuf,
    yaml: String,
}

fn prepare_yaml_delete(config_path: &Path, key: &str) -> Result<Option<PreparedYamlDelete>> {
    if !config_path.exists() {
        return Ok(None);
    }

    let mut config = load_mutable_yaml_config(config_path)?;

    if !delete_from_yaml(&mut config, key) {
        return Ok(None);
    }

    let yaml_str = serde_yml::to_string(&config)?;
    Ok(Some(PreparedYamlDelete {
        path: config_path.to_path_buf(),
        yaml: yaml_str,
    }))
}

fn apply_prepared_yaml_delete(prepared: Option<PreparedYamlDelete>) -> Result<bool> {
    let Some(prepared) = prepared else {
        return Ok(false);
    };

    // Atomic write: temp file + rename to prevent corruption on crash
    write_config_atomically(&prepared.path, &prepared.yaml)?;
    Ok(true)
}

fn delete_from_yaml(value: &mut serde_yml::Value, key: &str) -> bool {
    let parts: Vec<&str> = key.split('.').collect();
    delete_nested(value, &parts)
}

fn delete_nested(value: &mut serde_yml::Value, path: &[&str]) -> bool {
    let Some((part, remaining_path)) = path.split_first() else {
        return false;
    };

    if let serde_yml::Value::Mapping(map) = value {
        if remaining_path.is_empty() {
            let mut deleted = false;
            for alias in config_key_aliases(part) {
                let key = serde_yml::Value::String(alias);
                deleted |= map.remove(&key).is_some();
            }
            return deleted;
        }

        let mut deleted = false;
        for alias in config_key_aliases(part) {
            let key = serde_yml::Value::String(alias);
            if let Some(child) = map.get_mut(&key) {
                deleted |= delete_nested(child, remaining_path);
            }
        }
        return deleted;
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
            let value = layer.get(key).unwrap_or_default();
            let source = resolve_source(key, &layers);
            trace!(key, source = ?source, "Config source resolved");
            entries.push(ConfigEntry {
                key: key.clone(),
                value: format_config_value(value),
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
                let value = layer.get(key)?;
                Some(ConfigEntry {
                    key: key.clone(),
                    value: format_config_value(value),
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
                if let Some(value) = layer.get(key) {
                    println!("  {key}: {value}");
                }
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
        let result = set_config_value(&args, false, &CliOverrides::default(), &ctx);
        assert!(result.is_err());
    }

    #[test]
    fn test_nested_key_parsing() {
        // Test the key parsing logic - "display.color" should have 2 parts
        let parts: Vec<&str> = "display.color".split('.').collect();
        assert_eq!(parts.as_slice(), ["display", "color"]);
    }

    #[test]
    fn test_parse_editor_command_splits_flags() {
        let (program, args) = parse_editor_command("code --wait").unwrap();
        assert_eq!(program, "code");
        assert_eq!(args, vec!["--wait"]);
    }

    #[test]
    fn test_parse_editor_command_respects_quotes() {
        let (program, args) = parse_editor_command("\"my editor\" --flag").unwrap();
        assert_eq!(program, "my editor");
        assert_eq!(args, vec!["--flag"]);
    }

    #[test]
    fn test_allowed_editor_accepts_absolute_common_editor_path() {
        assert_eq!(
            AllowedEditor::from_program("/usr/bin/vim").unwrap(),
            AllowedEditor::Vim
        );
    }

    #[test]
    fn test_allowed_editor_accepts_common_visual_editor_aliases() {
        assert_eq!(
            AllowedEditor::from_program("/opt/homebrew/bin/nvim").unwrap(),
            AllowedEditor::Neovim
        );
        assert_eq!(
            AllowedEditor::from_program("emacsclient").unwrap(),
            AllowedEditor::EmacsClient
        );
        assert_eq!(
            AllowedEditor::from_program("cursor").unwrap(),
            AllowedEditor::Cursor
        );
    }

    #[test]
    fn test_allowed_editor_accepts_windows_exe_suffix() {
        assert_eq!(
            AllowedEditor::from_program("notepad.EXE").unwrap(),
            AllowedEditor::Notepad
        );
        assert_eq!(
            AllowedEditor::from_program(r"C:\Windows\System32\notepad.ExE").unwrap(),
            AllowedEditor::Notepad
        );
    }

    #[test]
    fn test_allowed_editor_rejects_unknown_program() {
        let err = AllowedEditor::from_program("custom-editor").unwrap_err();
        assert!(err.to_string().contains("Unsupported editor"));
    }

    #[test]
    fn test_delete_config_key_from_yaml_file_removes_value() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.yaml");
        fs::write(&config_path, "display:\n  color: true\n").unwrap();

        let deleted =
            apply_prepared_yaml_delete(prepare_yaml_delete(&config_path, "display.color").unwrap())
                .unwrap();
        assert!(deleted);

        let contents = fs::read_to_string(&config_path).unwrap();
        assert!(!contents.contains("color"));
    }

    #[test]
    fn test_delete_config_key_removes_hyphen_underscore_aliases() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.yaml");
        fs::write(
            &config_path,
            "issue-prefix: bd\nsync:\n  auto-flush: false\n",
        )
        .unwrap();

        let deleted_prefix =
            apply_prepared_yaml_delete(prepare_yaml_delete(&config_path, "issue_prefix").unwrap())
                .unwrap();
        assert!(deleted_prefix);

        let deleted_sync = apply_prepared_yaml_delete(
            prepare_yaml_delete(&config_path, "sync.auto_flush").unwrap(),
        )
        .unwrap();
        assert!(deleted_sync);

        let contents = fs::read_to_string(&config_path).unwrap();
        assert!(!contents.contains("issue-prefix"));
        assert!(!contents.contains("auto-flush"));
    }

    #[test]
    fn test_write_config_atomically_skips_stale_temp_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.yaml");
        fs::write(&config_path, "display:\n  color: true\n").unwrap();

        let stale_temp = config_path.with_extension(format!("yaml.{}.tmp", std::process::id()));
        fs::write(&stale_temp, "stale-temp-sentinel").unwrap();

        write_config_atomically(&config_path, "display:\n  color: false\n").unwrap();

        assert_eq!(
            fs::read_to_string(&stale_temp).unwrap(),
            "stale-temp-sentinel",
            "existing temp file should not be clobbered"
        );
        assert!(
            fs::read_to_string(&config_path)
                .unwrap()
                .contains("color: false"),
            "config should be updated via a fresh temp file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_write_config_atomically_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.yaml");
        fs::write(&config_path, "display:\n  color: true\n").unwrap();
        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();

        write_config_atomically(&config_path, "display:\n  color: false\n").unwrap();

        let mode = fs::metadata(&config_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "atomic rewrite should preserve file mode");
    }

    #[cfg(unix)]
    #[test]
    fn test_mutable_config_helpers_refuse_existing_symlink_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.yaml");
        let target_path = dir.path().join("outside-target.yaml");
        fs::write(&target_path, "display:\n  color: true\n").unwrap();
        symlink(&target_path, &config_path).unwrap();

        let load_err = load_mutable_yaml_config(&config_path).unwrap_err();
        assert!(load_err.to_string().contains("symbolic link"));

        let write_err =
            write_config_atomically(&config_path, "display:\n  color: false\n").unwrap_err();
        assert!(write_err.to_string().contains("symbolic link"));
        assert_eq!(
            fs::read_to_string(&target_path).unwrap(),
            "display:\n  color: true\n",
            "mutable config helpers must not read-edit-write a symlink target"
        );
        assert!(
            fs::symlink_metadata(&config_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "rejected config symlink should be left untouched"
        );
    }

    #[test]
    fn test_create_default_config_if_missing_creates_regular_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.yaml");
        let default_content = "display:\n  color: true\n";

        create_default_config_if_missing(&config_path, default_content).unwrap();

        assert_eq!(fs::read_to_string(&config_path).unwrap(), default_content);
        assert!(fs::symlink_metadata(&config_path).unwrap().is_file());
    }

    #[cfg(unix)]
    #[test]
    fn test_create_default_config_if_missing_refuses_dangling_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.yaml");
        let target_path = dir.path().join("outside-target.yaml");
        symlink(&target_path, &config_path).unwrap();

        let err = create_default_config_if_missing(&config_path, "display:\n  color: true\n")
            .unwrap_err();

        assert!(err.to_string().contains("symbolic link"));
        assert!(
            !target_path.exists(),
            "dangling symlink target must remain absent"
        );
        assert!(
            fs::symlink_metadata(&config_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "config path should not be replaced after refusal"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_create_default_config_if_missing_refuses_existing_symlink_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.yaml");
        let target_path = dir.path().join("outside-target.yaml");
        fs::write(&target_path, "existing-target").unwrap();
        symlink(&target_path, &config_path).unwrap();

        let err = create_default_config_if_missing(&config_path, "display:\n  color: true\n")
            .unwrap_err();

        assert!(err.to_string().contains("symbolic link"));
        assert_eq!(fs::read_to_string(&target_path).unwrap(), "existing-target");
        assert!(
            fs::symlink_metadata(&config_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "config path should not be replaced after refusal"
        );
    }

    #[test]
    fn test_build_layers_does_not_create_missing_db_for_read_only_access() {
        let dir = tempfile::TempDir::new().unwrap();
        let beads_dir = dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let layers = build_layers(Some(&beads_dir), &CliOverrides::default()).unwrap();

        assert!(
            !beads_dir.join("beads.db").exists(),
            "config read paths must not create a database as a side effect"
        );
        assert!(!layers.is_empty());
    }

    #[test]
    fn test_resolve_effective_db_path_reports_discovered_path_and_existence() {
        // #339: `config get db --json` must expose the RESOLVED canonical DB
        // path even with no explicit override, plus whether it exists.
        let dir = tempfile::TempDir::new().unwrap();
        let beads_dir = dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        // No override, no DB file yet: effective path points at the discovered
        // `.beads` DB location, and `exists` is false.
        let (path, exists) = resolve_effective_db_path(Some(&beads_dir), &CliOverrides::default());
        let path = path.expect("effective DB path resolved for discovered workspace");
        assert!(
            path.starts_with(crate::util::resolve_cache_dir(&beads_dir))
                || path.starts_with(&beads_dir),
            "effective path must resolve under the discovered .beads workspace: {}",
            path.display()
        );
        assert!(!exists, "DB does not exist yet");

        // Create the DB at the resolved path; `exists` flips to true.
        crate::storage::SqliteStorage::open(&path).unwrap();
        let (path2, exists2) =
            resolve_effective_db_path(Some(&beads_dir), &CliOverrides::default());
        assert_eq!(path2.as_ref(), Some(&path));
        assert!(exists2, "DB now exists at the resolved effective path");

        // No discovered workspace: returns (None, false).
        let (none_path, none_exists) = resolve_effective_db_path(None, &CliOverrides::default());
        assert!(none_path.is_none());
        assert!(!none_exists);
    }

    #[test]
    fn test_build_layers_reads_db_layer_from_startup_db_override() {
        let dir = tempfile::TempDir::new().unwrap();
        let beads_dir = dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(beads_dir.join("config.yaml"), "db: custom.db\n").unwrap();

        let db_path = crate::util::resolve_cache_dir(&beads_dir).join("custom.db");
        let mut storage = crate::storage::SqliteStorage::open(&db_path).unwrap();
        storage.set_config("issue_prefix", "proj").unwrap();

        let layers = build_layers(Some(&beads_dir), &CliOverrides::default()).unwrap();
        let db_layer = layers
            .iter()
            .find(|layer| matches!(layer.source, ConfigSource::Db))
            .expect("db layer");

        assert_eq!(
            db_layer
                .layer
                .runtime
                .get("issue_prefix")
                .map(String::as_str),
            Some("proj")
        );
    }

    #[test]
    fn test_build_layers_infers_prefix_from_jsonl_for_read_only_config_views() {
        let dir = tempfile::TempDir::new().unwrap();
        let beads_dir = dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(
            beads_dir.join("issues.jsonl"),
            r#"{"id":"proj-abc12","title":"Example"}"#,
        )
        .unwrap();

        let layers = build_layers(Some(&beads_dir), &CliOverrides::default()).unwrap();

        // Verify the JSONL layer directly to avoid interference from user
        // config files (~/.config/bd/config.yaml) that may override the prefix.
        let jsonl_layer = layers
            .iter()
            .find(|l| matches!(l.source, ConfigSource::Jsonl))
            .expect("should have a JSONL layer");
        assert_eq!(
            jsonl_layer
                .layer
                .runtime
                .get("issue_prefix")
                .map(String::as_str),
            Some("proj"),
            "JSONL layer should infer prefix from issue IDs"
        );
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
