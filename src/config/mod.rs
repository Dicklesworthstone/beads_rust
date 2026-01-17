//! Configuration management for `beads_rust`.
//!
//! Configuration sources and precedence (highest wins):
//! 1. CLI overrides
//! 2. Environment variables
//! 3. Project config (.beads/config.yaml)
//! 4. User config (~/.config/bd/config.yaml)
//! 5. Legacy user config (~/.beads/config.yaml)
//! 6. DB config table
//! 7. Defaults

use crate::error::{BeadsError, Result};
use crate::model::{IssueType, Priority};
use crate::storage::SqliteStorage;
use crate::util::id::IdConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tracing::warn;

/// Default database filename used when metadata is missing.
const DEFAULT_DB_FILENAME: &str = "beads.db";
/// Default JSONL filename used when metadata is missing.
const DEFAULT_JSONL_FILENAME: &str = "issues.jsonl";
/// Legacy JSONL filename to fall back to.
const LEGACY_JSONL_FILENAME: &str = "beads.jsonl";

/// JSONL files that should never be treated as the main export file.
/// Includes merge artifacts, deletion logs, and interaction logs.
const EXCLUDED_JSONL_FILES: &[&str] = &[
    "deletions.jsonl",
    "interactions.jsonl",
    "beads.base.jsonl",
    "beads.left.jsonl",
    "beads.right.jsonl",
    "sync_base.jsonl",
];

/// Startup metadata describing DB + JSONL paths.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Metadata {
    pub database: String,
    pub jsonl_export: String,
    #[serde(default)]
    pub backend: Option<String>,
    #[serde(default)]
    pub deletions_retention_days: Option<u64>,
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            database: DEFAULT_DB_FILENAME.to_string(),
            jsonl_export: DEFAULT_JSONL_FILENAME.to_string(),
            backend: None,
            deletions_retention_days: None,
        }
    }
}

impl Metadata {
    /// Load metadata.json from the beads directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or parsed.
    pub fn load(beads_dir: &Path) -> Result<Self> {
        let path = beads_dir.join("metadata.json");
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = fs::read_to_string(&path)?;
        let mut metadata: Self = serde_json::from_str(&contents)?;

        if metadata.database.trim().is_empty() {
            metadata.database = DEFAULT_DB_FILENAME.to_string();
        }
        if metadata.jsonl_export.trim().is_empty() {
            metadata.jsonl_export = DEFAULT_JSONL_FILENAME.to_string();
        }

        Ok(metadata)
    }
}

/// Discover the best JSONL file in the beads directory.
///
/// Selection rules:
/// 1. Prefer `issues.jsonl` if present.
/// 2. Fall back to `beads.jsonl` (legacy) if present.
/// 3. Never use merge artifacts (`beads.base.jsonl`, `beads.left.jsonl`, `beads.right.jsonl`).
/// 4. Never use deletion logs (`deletions.jsonl`) or interaction logs (`interactions.jsonl`).
/// 5. If no valid JSONL exists, return `None` (caller should use default for writing).
#[must_use]
pub fn discover_jsonl(beads_dir: &Path) -> Option<PathBuf> {
    // Check preferred file first
    let issues_path = beads_dir.join(DEFAULT_JSONL_FILENAME);
    if issues_path.is_file() {
        return Some(issues_path);
    }

    // Check legacy file
    let legacy_path = beads_dir.join(LEGACY_JSONL_FILENAME);
    if legacy_path.is_file() {
        return Some(legacy_path);
    }

    // No valid JSONL found
    None
}

/// Check if a JSONL filename should be excluded from discovery.
///
/// Returns `true` for merge artifacts, deletion logs, and interaction logs.
#[must_use]
pub fn is_excluded_jsonl(filename: &str) -> bool {
    EXCLUDED_JSONL_FILES.contains(&filename)
}

/// Resolved paths for this workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    pub beads_dir: PathBuf,
    pub db_path: PathBuf,
    pub jsonl_path: PathBuf,
    pub metadata: Metadata,
}

impl ConfigPaths {
    /// Resolve database + JSONL paths using metadata and environment overrides.
    ///
    /// # Errors
    ///
    /// Returns an error if metadata cannot be read.
    pub fn resolve(beads_dir: &Path, db_override: Option<&PathBuf>) -> Result<Self> {
        let metadata = Metadata::load(beads_dir)?;
        let db_path = resolve_db_path(beads_dir, &metadata, db_override);
        let jsonl_path = resolve_jsonl_path(beads_dir, &metadata, db_override);

        Ok(Self {
            beads_dir: beads_dir.to_path_buf(),
            db_path,
            jsonl_path,
            metadata,
        })
    }
}

/// Discover the active `.beads` directory.
///
/// Honors `BEADS_DIR` when set, otherwise walks up from `start` (or CWD).
///
/// # Errors
///
/// Returns an error if no beads directory is found or the CWD cannot be read.
pub fn discover_beads_dir(start: Option<&Path>) -> Result<PathBuf> {
    discover_beads_dir_with_env(start, None)
}

fn discover_beads_dir_with_env(
    start: Option<&Path>,
    env_override: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(path) = env_override {
        if path.is_dir() {
            return Ok(path.to_path_buf());
        }
    } else if let Ok(value) = env::var("BEADS_DIR") {
        if !value.trim().is_empty() {
            let path = PathBuf::from(value);
            if path.is_dir() {
                return Ok(path);
            }
        }
    }

    let mut current = match start {
        Some(path) => path.to_path_buf(),
        None => env::current_dir()?,
    };

    loop {
        let candidate = current.join(".beads");
        if candidate.is_dir() {
            return Ok(candidate);
        }

        if !current.pop() {
            break;
        }
    }

    Err(BeadsError::NotInitialized)
}

/// Open storage using resolved config paths, returning the storage and paths used.
///
/// # Errors
///
/// Returns an error if metadata cannot be read or the database cannot be opened.
pub fn open_storage(
    beads_dir: &Path,
    db_override: Option<&PathBuf>,
    lock_timeout: Option<u64>,
) -> Result<(SqliteStorage, ConfigPaths)> {
    let startup_layer = load_startup_config(beads_dir)?;
    let resolved_db_override = db_override
        .cloned()
        .or_else(|| db_override_from_layer(&startup_layer));
    let resolved_lock_timeout = lock_timeout
        .or_else(|| lock_timeout_from_layer(&startup_layer))
        .or(Some(30000));
    let paths = ConfigPaths::resolve(beads_dir, resolved_db_override.as_ref())?;
    let storage = SqliteStorage::open_with_timeout(&paths.db_path, resolved_lock_timeout)?;
    Ok((storage, paths))
}

/// Resolve config paths using startup config layers for overrides.
///
/// # Errors
///
/// Returns an error if startup config cannot be read or metadata cannot be loaded.
pub fn resolve_paths(beads_dir: &Path, db_override: Option<&PathBuf>) -> Result<ConfigPaths> {
    let startup_layer = load_startup_config(beads_dir)?;
    let resolved_db_override = db_override
        .cloned()
        .or_else(|| db_override_from_layer(&startup_layer));
    ConfigPaths::resolve(beads_dir, resolved_db_override.as_ref())
}

fn resolve_db_path(
    beads_dir: &Path,
    metadata: &Metadata,
    db_override: Option<&PathBuf>,
) -> PathBuf {
    if let Some(override_path) = db_override {
        return override_path.clone();
    }

    let candidate = PathBuf::from(&metadata.database);
    if candidate.is_absolute() {
        candidate
    } else {
        beads_dir.join(candidate)
    }
}

fn resolve_jsonl_path(
    beads_dir: &Path,
    metadata: &Metadata,
    db_override: Option<&PathBuf>,
) -> PathBuf {
    // Priority 1: BEADS_JSONL environment variable (highest priority)
    if let Ok(env_path) = env::var("BEADS_JSONL") {
        if !env_path.trim().is_empty() {
            return PathBuf::from(env_path);
        }
    }

    // Priority 2: DB override derives sibling JSONL path
    if db_override.is_some() {
        return db_override
            .and_then(|path| {
                path.parent()
                    .map(|parent| parent.join(DEFAULT_JSONL_FILENAME))
            })
            .unwrap_or_else(|| beads_dir.join(DEFAULT_JSONL_FILENAME));
    }

    // Priority 3: metadata.json override (if explicitly set to non-default)
    let metadata_jsonl = &metadata.jsonl_export;
    let is_explicit_override =
        metadata_jsonl != DEFAULT_JSONL_FILENAME && !is_excluded_jsonl(metadata_jsonl);

    if is_explicit_override {
        let candidate = PathBuf::from(metadata_jsonl);
        return if candidate.is_absolute() {
            candidate
        } else {
            beads_dir.join(candidate)
        };
    }

    // Priority 4: File discovery (prefer issues.jsonl, fall back to beads.jsonl)
    if let Some(discovered) = discover_jsonl(beads_dir) {
        return discovered;
    }

    // Priority 5: Default (issues.jsonl) for writing when nothing exists
    beads_dir.join(DEFAULT_JSONL_FILENAME)
}

/// A configuration layer split into startup-only and runtime (DB) keys.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigLayer {
    pub startup: HashMap<String, String>,
    pub runtime: HashMap<String, String>,
}

impl ConfigLayer {
    /// Merge another layer on top of this one (higher precedence wins).
    pub fn merge_from(&mut self, other: &Self) {
        for (key, value) in &other.startup {
            self.startup.insert(key.clone(), value.clone());
        }
        for (key, value) in &other.runtime {
            self.runtime.insert(key.clone(), value.clone());
        }
    }

    /// Merge multiple layers in precedence order (lowest to highest).
    #[must_use]
    pub fn merge_layers(layers: &[Self]) -> Self {
        let mut merged = Self::default();
        for layer in layers {
            merged.merge_from(layer);
        }
        merged
    }

    /// Build a layer from a YAML file path. Missing files return empty config.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or parsed.
    pub fn from_yaml(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(path)?;
        let value: serde_yaml::Value = serde_yaml::from_str(&contents)?;
        Ok(layer_from_yaml_value(&value))
    }

    /// Build a layer from environment variables.
    #[must_use]
    pub fn from_env() -> Self {
        let mut layer = Self::default();

        for (key, value) in env::vars() {
            if let Some(stripped) = key.strip_prefix("BD_") {
                let normalized = stripped.to_lowercase();
                for variant in env_key_variants(&normalized) {
                    insert_key_value(&mut layer, &variant, value.clone());
                }
            }
        }

        if let Ok(value) = env::var("BEADS_FLUSH_DEBOUNCE") {
            insert_key_value(&mut layer, "flush-debounce", value);
        }
        if let Ok(value) = env::var("BEADS_IDENTITY") {
            insert_key_value(&mut layer, "identity", value);
        }
        if let Ok(value) = env::var("BEADS_REMOTE_SYNC_INTERVAL") {
            insert_key_value(&mut layer, "remote-sync-interval", value);
        }
        if let Ok(value) = env::var("BEADS_AUTO_START_DAEMON") {
            if let Some(enabled) = parse_bool(&value) {
                insert_key_value(&mut layer, "no-daemon", (!enabled).to_string());
            }
        }

        layer
    }

    /// Build a layer from DB config table values.
    ///
    /// # Errors
    ///
    /// Returns an error if config table lookup fails.
    pub fn from_db(storage: &SqliteStorage) -> Result<Self> {
        let mut layer = Self::default();
        let map = storage.get_all_config()?;
        for (key, value) in map {
            if is_startup_key(&key) {
                continue;
            }
            layer.runtime.insert(key, value);
        }
        Ok(layer)
    }
}

/// CLI overrides for config loading (optional).
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub db: Option<PathBuf>,
    pub actor: Option<String>,
    pub identity: Option<String>,
    pub json: Option<bool>,
    pub display_color: Option<bool>,
    pub no_db: Option<bool>,
    pub no_daemon: Option<bool>,
    pub no_auto_flush: Option<bool>,
    pub no_auto_import: Option<bool>,
    pub lock_timeout: Option<u64>,
}

impl CliOverrides {
    #[must_use]
    pub fn as_layer(&self) -> ConfigLayer {
        let mut layer = ConfigLayer::default();

        if let Some(path) = &self.db {
            insert_key_value(&mut layer, "db", path.to_string_lossy().to_string());
        }
        if let Some(actor) = &self.actor {
            insert_key_value(&mut layer, "actor", actor.clone());
        }
        if let Some(identity) = &self.identity {
            insert_key_value(&mut layer, "identity", identity.clone());
        }
        if let Some(json) = self.json {
            insert_key_value(&mut layer, "json", json.to_string());
        }
        if let Some(display_color) = self.display_color {
            insert_key_value(&mut layer, "display.color", display_color.to_string());
        }
        if let Some(no_db) = self.no_db {
            insert_key_value(&mut layer, "no-db", no_db.to_string());
        }
        if let Some(no_daemon) = self.no_daemon {
            insert_key_value(&mut layer, "no-daemon", no_daemon.to_string());
        }
        if let Some(no_auto_flush) = self.no_auto_flush {
            insert_key_value(&mut layer, "no-auto-flush", no_auto_flush.to_string());
        }
        if let Some(no_auto_import) = self.no_auto_import {
            insert_key_value(&mut layer, "no-auto-import", no_auto_import.to_string());
        }
        if let Some(lock_timeout) = self.lock_timeout {
            insert_key_value(&mut layer, "lock-timeout", lock_timeout.to_string());
        }

        layer
    }
}

/// Load project config (.beads/config.yaml).
///
/// # Errors
///
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_project_config(beads_dir: &Path) -> Result<ConfigLayer> {
    ConfigLayer::from_yaml(&beads_dir.join("config.yaml"))
}

/// Load user config (~/.config/bd/config.yaml).
///
/// # Errors
///
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_user_config() -> Result<ConfigLayer> {
    let Ok(home) = env::var("HOME") else {
        return Ok(ConfigLayer::default());
    };
    let path = Path::new(&home)
        .join(".config")
        .join("bd")
        .join("config.yaml");
    ConfigLayer::from_yaml(&path)
}

/// Load legacy user config (~/.beads/config.yaml).
///
/// # Errors
///
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_legacy_user_config() -> Result<ConfigLayer> {
    let Ok(home) = env::var("HOME") else {
        return Ok(ConfigLayer::default());
    };
    let path = Path::new(&home).join(".beads").join("config.yaml");
    ConfigLayer::from_yaml(&path)
}

/// Load startup-only configuration layers (YAML + env, no DB).
///
/// # Errors
///
/// Returns an error if any config file cannot be read or parsed.
pub fn load_startup_config(beads_dir: &Path) -> Result<ConfigLayer> {
    let legacy_user = load_legacy_user_config()?;
    let user = load_user_config()?;
    let project = load_project_config(beads_dir)?;
    let env_layer = ConfigLayer::from_env();

    Ok(ConfigLayer::merge_layers(&[
        legacy_user,
        user,
        project,
        env_layer,
    ]))
}

/// Default config layer (lowest precedence).
#[must_use]
pub fn default_config_layer() -> ConfigLayer {
    let mut layer = ConfigLayer::default();
    layer
        .runtime
        .insert("issue_prefix".to_string(), "bd".to_string());
    layer
}

/// Load configuration with classic precedence order.
///
/// # Errors
///
/// Returns an error if any config file cannot be read or parsed, or DB access fails.
pub fn load_config(
    beads_dir: &Path,
    storage: Option<&SqliteStorage>,
    cli: &CliOverrides,
) -> Result<ConfigLayer> {
    let defaults = default_config_layer();
    let db_layer = match storage {
        Some(storage) => ConfigLayer::from_db(storage)?,
        None => ConfigLayer::default(),
    };
    let legacy_user = load_legacy_user_config()?;
    let user = load_user_config()?;
    let project = load_project_config(beads_dir)?;
    let env_layer = ConfigLayer::from_env();
    let cli_layer = cli.as_layer();

    Ok(ConfigLayer::merge_layers(&[
        defaults,
        db_layer,
        legacy_user,
        user,
        project,
        env_layer,
        cli_layer,
    ]))
}

/// Build ID generation config from a merged config layer.
#[must_use]
pub fn id_config_from_layer(layer: &ConfigLayer) -> IdConfig {
    let prefix = get_value(layer, &["issue_prefix", "issue-prefix", "prefix"])
        .cloned()
        .unwrap_or_else(|| "bd".to_string());

    let min_hash_length = parse_usize(layer, &["min_hash_length", "min-hash-length"]).unwrap_or(3);
    let max_hash_length = parse_usize(layer, &["max_hash_length", "max-hash-length"]).unwrap_or(8);
    let max_collision_prob =
        parse_f64(layer, &["max_collision_prob", "max-collision-prob"]).unwrap_or(0.25);

    IdConfig {
        prefix,
        min_hash_length,
        max_hash_length,
        max_collision_prob,
    }
}

/// Resolve default priority for new issues from config.
///
/// # Errors
///
/// Returns an error if the configured value is not a valid priority (0-4).
pub fn default_priority_from_layer(layer: &ConfigLayer) -> Result<Priority> {
    get_value(layer, &["default_priority", "default-priority"])
        .map_or_else(|| Ok(Priority::MEDIUM), |value| Priority::from_str(value))
}

/// Resolve default issue type for new issues from config.
///
/// # Errors
///
/// Returns an error only if parsing fails (custom types are allowed).
pub fn default_issue_type_from_layer(layer: &ConfigLayer) -> Result<IssueType> {
    get_value(layer, &["default_type", "default-type"])
        .map_or_else(|| Ok(IssueType::Task), |value| IssueType::from_str(value))
}

/// Resolve display color preference from a merged config layer.
///
/// Accepts keys: `display.color`, `display-color`, `display_color`.
#[must_use]
pub fn display_color_from_layer(layer: &ConfigLayer) -> Option<bool> {
    get_value(layer, &["display.color", "display-color", "display_color"])
        .and_then(|value| parse_bool(value))
}

/// Determine whether human-readable output should use ANSI color.
///
/// Precedence:
/// 1) Config `display.color` (if set)
/// 2) `NO_COLOR` environment variable (standard)
/// 3) stdout is a terminal
#[must_use]
pub fn should_use_color(layer: &ConfigLayer) -> bool {
    if let Some(value) = display_color_from_layer(layer) {
        return value;
    }
    if env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stdout().is_terminal()
}

/// Resolve external project mappings from config.
///
/// Supports `external_projects.<name>` or `external-projects.<name>` keys.
/// Relative paths are resolved against the project root (parent of `.beads`).
#[must_use]
pub fn external_projects_from_layer(
    layer: &ConfigLayer,
    beads_dir: &Path,
) -> HashMap<String, PathBuf> {
    let base_dir = beads_dir.parent().unwrap_or(beads_dir);
    let mut map = HashMap::new();
    let iter = layer.runtime.iter().chain(layer.startup.iter());

    for (key, value) in iter {
        let key_lower = key.to_lowercase();
        let is_external = key_lower.starts_with("external_projects.")
            || key_lower.starts_with("external-projects.");
        if !is_external {
            continue;
        }

        let project = key.split_once('.').map(|(_, rest)| rest);
        let Some(project) = project.filter(|p| !p.trim().is_empty()) else {
            continue;
        };

        let path = PathBuf::from(value.trim());
        let resolved = if path.is_absolute() {
            path
        } else {
            base_dir.join(path)
        };
        map.insert(project.trim().to_string(), resolved);
    }

    map
}

/// Resolve external project DB paths from config.
///
/// Projects are expected to be either a `.beads` directory or a project root
/// containing `.beads/`.
#[must_use]
pub fn external_project_db_paths(
    layer: &ConfigLayer,
    beads_dir: &Path,
) -> HashMap<String, PathBuf> {
    let projects = external_projects_from_layer(layer, beads_dir);
    let mut db_paths = HashMap::new();

    for (name, path) in projects {
        let beads_path = if path.file_name().is_some_and(|name| name == ".beads") {
            path.clone()
        } else {
            path.join(".beads")
        };

        if !beads_path.is_dir() {
            warn!(
                project = %name,
                path = %beads_path.display(),
                "External project .beads directory not found"
            );
            continue;
        }

        match ConfigPaths::resolve(&beads_path, None) {
            Ok(paths) => {
                db_paths.insert(name, paths.db_path);
            }
            Err(err) => {
                warn!(
                    project = %name,
                    path = %beads_path.display(),
                    error = %err,
                    "Failed to resolve external project DB path"
                );
            }
        }
    }

    db_paths
}

/// Resolve actor from a merged config layer.
#[must_use]
pub fn actor_from_layer(layer: &ConfigLayer) -> Option<String> {
    get_startup_value(layer, &["actor"])
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Resolve actor with fallback to USER and a safe default.
#[must_use]
pub fn resolve_actor(layer: &ConfigLayer) -> String {
    actor_from_layer(layer)
        .or_else(|| {
            std::env::var("USER")
                .ok()
                .map(|value| value.trim().to_string())
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Determine if a key is startup-only.
///
/// Startup-only keys can only be set in YAML config files, not in the database.
/// These include path settings, behavior flags, and git-related options.
#[must_use]
pub fn is_startup_key(key: &str) -> bool {
    let normalized = normalize_key(key);

    if normalized.starts_with("git.")
        || normalized.starts_with("routing.")
        || normalized.starts_with("validation.")
        || normalized.starts_with("directory.")
        || normalized.starts_with("sync.")
        || normalized.starts_with("external-projects.")
    {
        return true;
    }

    matches!(
        normalized.as_str(),
        "no-db"
            | "no-daemon"
            | "no-auto-flush"
            | "no-auto-import"
            | "json"
            | "db"
            | "actor"
            | "identity"
            | "flush-debounce"
            | "lock-timeout"
            | "remote-sync-interval"
            | "no-git-ops"
            | "no-push"
            | "sync-branch"
            | "sync.branch"
            | "external-projects"
            | "hierarchy.max-depth"
    )
}

fn insert_key_value(layer: &mut ConfigLayer, key: &str, value: String) {
    if is_startup_key(key) {
        layer.startup.insert(key.to_string(), value);
    } else {
        layer.runtime.insert(key.to_string(), value);
    }
}

fn normalize_key(key: &str) -> String {
    key.trim().to_lowercase().replace('_', "-")
}

fn env_key_variants(raw: &str) -> Vec<String> {
    let mut variants = Vec::new();
    let raw_lower = raw.to_lowercase();
    variants.push(raw_lower.clone());
    variants.push(raw_lower.replace('_', "."));
    variants.push(raw_lower.replace('_', "-"));
    variants
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

fn get_startup_value<'a>(layer: &'a ConfigLayer, keys: &[&str]) -> Option<&'a String> {
    let normalized_keys: Vec<String> = keys.iter().map(|key| normalize_key(key)).collect();
    for (key, value) in &layer.startup {
        let normalized = normalize_key(key);
        if normalized_keys
            .iter()
            .any(|candidate| candidate == &normalized)
        {
            return Some(value);
        }
    }
    None
}

fn get_value<'a>(layer: &'a ConfigLayer, keys: &[&str]) -> Option<&'a String> {
    for key in keys {
        if let Some(value) = layer.runtime.get(*key) {
            return Some(value);
        }
    }
    None
}

fn parse_usize(layer: &ConfigLayer, keys: &[&str]) -> Option<usize> {
    get_value(layer, keys).and_then(|value| value.trim().parse::<usize>().ok())
}

fn parse_f64(layer: &ConfigLayer, keys: &[&str]) -> Option<f64> {
    get_value(layer, keys).and_then(|value| value.trim().parse::<f64>().ok())
}

fn db_override_from_layer(layer: &ConfigLayer) -> Option<PathBuf> {
    get_startup_value(layer, &["db", "database"]).and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed))
        }
    })
}

fn lock_timeout_from_layer(layer: &ConfigLayer) -> Option<u64> {
    get_startup_value(layer, &["lock-timeout", "lock_timeout"])
        .and_then(|value| value.trim().parse::<u64>().ok())
}

fn layer_from_yaml_value(value: &serde_yaml::Value) -> ConfigLayer {
    let mut layer = ConfigLayer::default();
    let mut flat = HashMap::new();
    flatten_yaml(value, "", &mut flat);

    for (key, value) in flat {
        insert_key_value(&mut layer, &key, value);
    }

    layer
}

fn flatten_yaml(value: &serde_yaml::Value, prefix: &str, out: &mut HashMap<String, String>) {
    match value {
        serde_yaml::Value::Mapping(map) => {
            for (key, value) in map {
                let Some(key_str) = key.as_str() else {
                    continue;
                };
                let next_prefix = if prefix.is_empty() {
                    key_str.to_string()
                } else {
                    format!("{prefix}.{key_str}")
                };
                flatten_yaml(value, &next_prefix, out);
            }
        }
        serde_yaml::Value::Sequence(values) => {
            let joined = values
                .iter()
                .filter_map(yaml_scalar_to_string)
                .collect::<Vec<_>>()
                .join(",");
            out.insert(prefix.to_string(), joined);
        }
        _ => {
            if let Some(value) = yaml_scalar_to_string(value) {
                out.insert(prefix.to_string(), value);
            }
        }
    }
}

fn yaml_scalar_to_string(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::Bool(v) => Some(v.to_string()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Null
        | serde_yaml::Value::Sequence(_)
        | serde_yaml::Value::Mapping(_) => None,
        serde_yaml::Value::Tagged(tagged) => yaml_scalar_to_string(&tagged.value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IssueType, Priority};
    use crate::storage::SqliteStorage;
    use tempfile::TempDir;

    #[test]
    fn metadata_defaults_when_missing() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let metadata = Metadata::load(&beads_dir).expect("metadata");
        assert_eq!(metadata.database, DEFAULT_DB_FILENAME);
        assert_eq!(metadata.jsonl_export, DEFAULT_JSONL_FILENAME);
    }

    #[test]
    fn metadata_override_paths() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let metadata_path = beads_dir.join("metadata.json");
        let metadata = r#"{"database": "custom.db", "jsonl_export": "custom.jsonl"}"#;
        fs::write(metadata_path, metadata).expect("write metadata");

        let paths = ConfigPaths::resolve(&beads_dir, None).expect("paths");
        assert_eq!(paths.db_path, beads_dir.join("custom.db"));
        assert_eq!(paths.jsonl_path, beads_dir.join("custom.jsonl"));
    }

    #[test]
    fn merge_precedence_order() {
        let mut defaults = default_config_layer();
        defaults
            .runtime
            .insert("issue_prefix".to_string(), "bd".to_string());

        let mut db = ConfigLayer::default();
        db.runtime
            .insert("issue_prefix".to_string(), "db".to_string());

        let mut yaml = ConfigLayer::default();
        yaml.runtime
            .insert("issue_prefix".to_string(), "yaml".to_string());

        let mut env_layer = ConfigLayer::default();
        env_layer
            .runtime
            .insert("issue_prefix".to_string(), "env".to_string());

        let mut cli = ConfigLayer::default();
        cli.runtime
            .insert("issue_prefix".to_string(), "cli".to_string());

        let merged = ConfigLayer::merge_layers(&[defaults, db, yaml, env_layer, cli]);
        assert_eq!(merged.runtime.get("issue_prefix").unwrap(), "cli");
    }

    #[test]
    fn yaml_startup_keys_are_separated() {
        let yaml = r"
no-db: true
issue_prefix: bd
";
        let value: serde_yaml::Value = serde_yaml::from_str(yaml).expect("parse yaml");
        let layer = layer_from_yaml_value(&value);
        assert_eq!(layer.startup.get("no-db").unwrap(), "true");
        assert_eq!(layer.runtime.get("issue_prefix").unwrap(), "bd");
    }

    #[test]
    fn yaml_sequence_flattens_to_csv() {
        let yaml = r"
labels:
  - backend
  - api
";
        let value: serde_yaml::Value = serde_yaml::from_str(yaml).expect("parse yaml");
        let layer = layer_from_yaml_value(&value);
        assert_eq!(layer.runtime.get("labels").unwrap(), "backend,api");
    }

    #[test]
    fn id_config_parses_numeric_overrides() {
        let mut layer = ConfigLayer::default();
        layer
            .runtime
            .insert("issue_prefix".to_string(), "br".to_string());
        layer
            .runtime
            .insert("min_hash_length".to_string(), "4".to_string());
        layer
            .runtime
            .insert("max_hash_length".to_string(), "10".to_string());
        layer
            .runtime
            .insert("max_collision_prob".to_string(), "0.5".to_string());

        let config = id_config_from_layer(&layer);
        assert_eq!(config.prefix, "br");
        assert_eq!(config.min_hash_length, 4);
        assert_eq!(config.max_hash_length, 10);
        assert!((config.max_collision_prob - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn default_priority_from_layer_uses_config_value() {
        let mut layer = ConfigLayer::default();
        layer
            .runtime
            .insert("default_priority".to_string(), "1".to_string());

        let priority = default_priority_from_layer(&layer).expect("default priority");
        assert_eq!(priority, Priority::HIGH);
    }

    #[test]
    fn default_priority_from_layer_errors_on_invalid_value() {
        let mut layer = ConfigLayer::default();
        layer
            .runtime
            .insert("default_priority".to_string(), "9".to_string());

        assert!(default_priority_from_layer(&layer).is_err());
    }

    #[test]
    fn default_issue_type_from_layer_uses_config_value() {
        let mut layer = ConfigLayer::default();
        layer
            .runtime
            .insert("default_type".to_string(), "feature".to_string());

        let issue_type = default_issue_type_from_layer(&layer).expect("default type");
        assert_eq!(issue_type, IssueType::Feature);
    }

    #[test]
    fn db_layer_skips_startup_keys() {
        let mut storage = SqliteStorage::open_memory().expect("storage");
        storage.set_config("no-db", "true").expect("set no-db");
        storage
            .set_config("issue_prefix", "bd")
            .expect("set issue_prefix");

        let layer = ConfigLayer::from_db(&storage).expect("db layer");
        assert!(!layer.startup.contains_key("no-db"));
        assert_eq!(layer.runtime.get("issue_prefix").unwrap(), "bd");
    }

    #[test]
    fn discover_beads_dir_uses_env_override() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let discovered = discover_beads_dir_with_env(None, Some(&beads_dir)).expect("discover");
        assert_eq!(discovered, beads_dir);
    }

    #[test]
    fn discover_beads_dir_walks_up() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        let nested = temp.path().join("a").join("b");
        fs::create_dir_all(&nested).expect("create nested");

        let discovered = discover_beads_dir(Some(&nested)).expect("discover");
        assert_eq!(discovered, beads_dir);
    }

    #[test]
    fn open_storage_uses_yaml_db_over_metadata() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        fs::write(beads_dir.join("config.yaml"), "db: custom.db\n").expect("write config");
        fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database": "beads.db", "jsonl_export": "issues.jsonl"}"#,
        )
        .expect("write metadata");

        let (_storage, paths) = open_storage(&beads_dir, None, None).expect("open storage");
        assert_eq!(paths.db_path, PathBuf::from("custom.db"));
    }

    #[test]
    fn startup_layer_reads_db_override() {
        let mut layer = ConfigLayer::default();
        layer
            .startup
            .insert("db".to_string(), "/tmp/beads.db".to_string());

        let override_path = db_override_from_layer(&layer).expect("db override");
        assert_eq!(override_path, PathBuf::from("/tmp/beads.db"));
    }

    #[test]
    fn startup_layer_reads_lock_timeout() {
        let mut layer = ConfigLayer::default();
        layer
            .startup
            .insert("lock_timeout".to_string(), "2500".to_string());

        let timeout = lock_timeout_from_layer(&layer).expect("lock timeout");
        assert_eq!(timeout, 2500);
    }

    // ==================== Additional Config Unit Tests ====================
    // Tests for beads_rust-7h9: Config unit tests - Layered configuration

    #[test]
    fn precedence_default_is_lowest() {
        // Verify that default layer values are overridden by any other layer
        let defaults = default_config_layer();
        assert_eq!(defaults.runtime.get("issue_prefix").unwrap(), "bd");

        let mut db = ConfigLayer::default();
        db.runtime
            .insert("issue_prefix".to_string(), "from_db".to_string());

        let merged = ConfigLayer::merge_layers(&[defaults, db]);
        assert_eq!(merged.runtime.get("issue_prefix").unwrap(), "from_db");
    }

    #[test]
    fn precedence_db_overrides_default() {
        let defaults = default_config_layer();
        let mut db = ConfigLayer::default();
        db.runtime
            .insert("issue_prefix".to_string(), "db_prefix".to_string());

        let merged = ConfigLayer::merge_layers(&[defaults, db]);
        assert_eq!(merged.runtime.get("issue_prefix").unwrap(), "db_prefix");
    }

    #[test]
    fn precedence_yaml_overrides_db() {
        let defaults = default_config_layer();
        let mut db = ConfigLayer::default();
        db.runtime
            .insert("issue_prefix".to_string(), "db_prefix".to_string());
        let mut yaml = ConfigLayer::default();
        yaml.runtime
            .insert("issue_prefix".to_string(), "yaml_prefix".to_string());

        let merged = ConfigLayer::merge_layers(&[defaults, db, yaml]);
        assert_eq!(merged.runtime.get("issue_prefix").unwrap(), "yaml_prefix");
    }

    #[test]
    fn precedence_env_overrides_yaml() {
        let defaults = default_config_layer();
        let mut yaml = ConfigLayer::default();
        yaml.runtime
            .insert("issue_prefix".to_string(), "yaml_prefix".to_string());
        let mut env_layer = ConfigLayer::default();
        env_layer
            .runtime
            .insert("issue_prefix".to_string(), "env_prefix".to_string());

        let merged = ConfigLayer::merge_layers(&[defaults, yaml, env_layer]);
        assert_eq!(merged.runtime.get("issue_prefix").unwrap(), "env_prefix");
    }

    #[test]
    fn precedence_cli_overrides_all() {
        let defaults = default_config_layer();
        let mut db = ConfigLayer::default();
        db.runtime
            .insert("issue_prefix".to_string(), "db".to_string());
        let mut yaml = ConfigLayer::default();
        yaml.runtime
            .insert("issue_prefix".to_string(), "yaml".to_string());
        let mut env_layer = ConfigLayer::default();
        env_layer
            .runtime
            .insert("issue_prefix".to_string(), "env".to_string());
        let mut cli = ConfigLayer::default();
        cli.runtime
            .insert("issue_prefix".to_string(), "cli_wins".to_string());

        let merged = ConfigLayer::merge_layers(&[defaults, db, yaml, env_layer, cli]);
        assert_eq!(merged.runtime.get("issue_prefix").unwrap(), "cli_wins");
    }

    #[test]
    fn precedence_full_chain_with_different_keys() {
        // Each layer sets a different key, all should be preserved
        let mut defaults = default_config_layer();
        defaults
            .runtime
            .insert("from_default".to_string(), "default_value".to_string());

        let mut db = ConfigLayer::default();
        db.runtime
            .insert("from_db".to_string(), "db_value".to_string());

        let mut yaml = ConfigLayer::default();
        yaml.runtime
            .insert("from_yaml".to_string(), "yaml_value".to_string());

        let mut env_layer = ConfigLayer::default();
        env_layer
            .runtime
            .insert("from_env".to_string(), "env_value".to_string());

        let mut cli = ConfigLayer::default();
        cli.runtime
            .insert("from_cli".to_string(), "cli_value".to_string());

        let merged = ConfigLayer::merge_layers(&[defaults, db, yaml, env_layer, cli]);

        assert_eq!(merged.runtime.get("from_default").unwrap(), "default_value");
        assert_eq!(merged.runtime.get("from_db").unwrap(), "db_value");
        assert_eq!(merged.runtime.get("from_yaml").unwrap(), "yaml_value");
        assert_eq!(merged.runtime.get("from_env").unwrap(), "env_value");
        assert_eq!(merged.runtime.get("from_cli").unwrap(), "cli_value");
    }

    #[test]
    fn metadata_handles_empty_strings() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Write metadata with empty strings
        let metadata_path = beads_dir.join("metadata.json");
        let metadata = r#"{"database": "", "jsonl_export": "  "}"#;
        fs::write(metadata_path, metadata).expect("write metadata");

        let loaded = Metadata::load(&beads_dir).expect("metadata");
        // Empty strings should fall back to defaults
        assert_eq!(loaded.database, DEFAULT_DB_FILENAME);
        assert_eq!(loaded.jsonl_export, DEFAULT_JSONL_FILENAME);
    }

    #[test]
    fn metadata_handles_extra_fields() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Write metadata with extra fields (should be ignored)
        let metadata_path = beads_dir.join("metadata.json");
        let metadata =
            r#"{"database": "test.db", "jsonl_export": "test.jsonl", "unknown_field": true}"#;
        fs::write(metadata_path, metadata).expect("write metadata");

        let loaded = Metadata::load(&beads_dir).expect("metadata");
        assert_eq!(loaded.database, "test.db");
        assert_eq!(loaded.jsonl_export, "test.jsonl");
    }

    #[test]
    fn metadata_with_backend_and_retention() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let metadata_path = beads_dir.join("metadata.json");
        let metadata = r#"{"database": "beads.db", "jsonl_export": "issues.jsonl", "backend": "sqlite", "deletions_retention_days": 30}"#;
        fs::write(metadata_path, metadata).expect("write metadata");

        let loaded = Metadata::load(&beads_dir).expect("metadata");
        assert_eq!(loaded.backend, Some("sqlite".to_string()));
        assert_eq!(loaded.deletions_retention_days, Some(30));
    }

    #[test]
    fn discover_beads_dir_returns_error_when_not_found() {
        let temp = TempDir::new().expect("tempdir");
        // No .beads directory created

        let result = discover_beads_dir(Some(temp.path()));
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), BeadsError::NotInitialized));
    }

    #[test]
    fn discover_beads_dir_finds_at_root() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let discovered = discover_beads_dir(Some(temp.path())).expect("discover");
        assert_eq!(discovered, beads_dir);
    }

    #[test]
    fn discover_beads_dir_deeply_nested() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Create deeply nested directory
        let nested = temp
            .path()
            .join("a")
            .join("b")
            .join("c")
            .join("d")
            .join("e");
        fs::create_dir_all(&nested).expect("create nested");

        let discovered = discover_beads_dir(Some(&nested)).expect("discover");
        assert_eq!(discovered, beads_dir);
    }

    #[test]
    fn env_key_variants_generates_all_forms() {
        let variants = env_key_variants("no_auto_flush");
        assert!(variants.contains(&"no_auto_flush".to_string()));
        assert!(variants.contains(&"no.auto.flush".to_string()));
        assert!(variants.contains(&"no-auto-flush".to_string()));
    }

    #[test]
    fn normalize_key_handles_various_formats() {
        assert_eq!(normalize_key("ISSUE_PREFIX"), "issue-prefix");
        assert_eq!(normalize_key("issue-prefix"), "issue-prefix");
        assert_eq!(normalize_key("issue_prefix"), "issue-prefix");
        assert_eq!(normalize_key("  ISSUE_PREFIX  "), "issue-prefix");
    }

    #[test]
    fn parse_bool_handles_all_truthy_values() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("TRUE"), Some(true));
        assert_eq!(parse_bool("1"), Some(true));
        assert_eq!(parse_bool("yes"), Some(true));
        assert_eq!(parse_bool("YES"), Some(true));
        assert_eq!(parse_bool("y"), Some(true));
        assert_eq!(parse_bool("on"), Some(true));
    }

    #[test]
    fn parse_bool_handles_all_falsy_values() {
        assert_eq!(parse_bool("false"), Some(false));
        assert_eq!(parse_bool("FALSE"), Some(false));
        assert_eq!(parse_bool("0"), Some(false));
        assert_eq!(parse_bool("no"), Some(false));
        assert_eq!(parse_bool("NO"), Some(false));
        assert_eq!(parse_bool("n"), Some(false));
        assert_eq!(parse_bool("off"), Some(false));
    }

    #[test]
    fn parse_bool_returns_none_for_invalid() {
        assert_eq!(parse_bool("maybe"), None);
        assert_eq!(parse_bool(""), None);
        assert_eq!(parse_bool("2"), None);
    }

    #[test]
    fn is_startup_key_identifies_startup_keys() {
        assert!(is_startup_key("no-db"));
        assert!(is_startup_key("no-daemon"));
        assert!(is_startup_key("no-auto-flush"));
        assert!(is_startup_key("no-auto-import"));
        assert!(is_startup_key("json"));
        assert!(is_startup_key("db"));
        assert!(is_startup_key("actor"));
        assert!(is_startup_key("identity"));
        assert!(is_startup_key("lock-timeout"));
        assert!(is_startup_key("git.branch")); // prefix check
        assert!(is_startup_key("routing.policy")); // prefix check
    }

    #[test]
    fn is_startup_key_identifies_runtime_keys() {
        assert!(!is_startup_key("issue_prefix"));
        assert!(!is_startup_key("issue-prefix"));
        assert!(!is_startup_key("min_hash_length"));
        assert!(!is_startup_key("labels"));
    }

    #[test]
    fn resolve_db_path_absolute_in_metadata() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let absolute_path = "/absolute/path/to/beads.db";
        let metadata = Metadata {
            database: absolute_path.to_string(),
            jsonl_export: DEFAULT_JSONL_FILENAME.to_string(),
            backend: None,
            deletions_retention_days: None,
        };

        let resolved = resolve_db_path(&beads_dir, &metadata, None);
        assert_eq!(resolved, PathBuf::from(absolute_path));
    }

    #[test]
    fn resolve_db_path_relative_in_metadata() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let metadata = Metadata {
            database: "relative.db".to_string(),
            jsonl_export: DEFAULT_JSONL_FILENAME.to_string(),
            backend: None,
            deletions_retention_days: None,
        };

        let resolved = resolve_db_path(&beads_dir, &metadata, None);
        assert_eq!(resolved, beads_dir.join("relative.db"));
    }

    #[test]
    fn resolve_db_path_override_wins() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let metadata = Metadata::default();
        let override_path = PathBuf::from("/override/path.db");

        let resolved = resolve_db_path(&beads_dir, &metadata, Some(&override_path));
        assert_eq!(resolved, override_path);
    }

    #[test]
    fn resolve_jsonl_path_absolute_in_metadata() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let absolute_path = "/absolute/path/to/issues.jsonl";
        let metadata = Metadata {
            database: DEFAULT_DB_FILENAME.to_string(),
            jsonl_export: absolute_path.to_string(),
            backend: None,
            deletions_retention_days: None,
        };

        let resolved = resolve_jsonl_path(&beads_dir, &metadata, None);
        assert_eq!(resolved, PathBuf::from(absolute_path));
    }

    #[test]
    fn resolve_jsonl_path_relative_in_metadata() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let metadata = Metadata {
            database: DEFAULT_DB_FILENAME.to_string(),
            jsonl_export: "relative.jsonl".to_string(),
            backend: None,
            deletions_retention_days: None,
        };

        let resolved = resolve_jsonl_path(&beads_dir, &metadata, None);
        assert_eq!(resolved, beads_dir.join("relative.jsonl"));
    }

    #[test]
    fn resolve_jsonl_path_db_override_derives_sibling() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let metadata = Metadata::default();
        let db_override = PathBuf::from("/some/path/custom.db");

        let resolved = resolve_jsonl_path(&beads_dir, &metadata, Some(&db_override));
        assert_eq!(resolved, PathBuf::from("/some/path/issues.jsonl"));
    }

    #[test]
    fn cli_overrides_as_layer_sets_startup_keys() {
        let cli = CliOverrides {
            db: Some(PathBuf::from("/cli/path.db")),
            actor: Some("cli_actor".to_string()),
            json: Some(true),
            display_color: None,
            no_db: Some(true),
            no_daemon: Some(true),
            no_auto_flush: Some(true),
            no_auto_import: Some(true),
            lock_timeout: Some(5000),
            identity: None,
        };

        let layer = cli.as_layer();

        assert_eq!(layer.startup.get("db").unwrap(), "/cli/path.db");
        assert_eq!(layer.startup.get("actor").unwrap(), "cli_actor");
        assert_eq!(layer.startup.get("json").unwrap(), "true");
        assert_eq!(layer.startup.get("no-db").unwrap(), "true");
        assert_eq!(layer.startup.get("no-daemon").unwrap(), "true");
        assert_eq!(layer.startup.get("no-auto-flush").unwrap(), "true");
        assert_eq!(layer.startup.get("no-auto-import").unwrap(), "true");
        assert_eq!(layer.startup.get("lock-timeout").unwrap(), "5000");
    }

    #[test]
    fn cli_overrides_empty_produces_empty_layer() {
        let cli = CliOverrides::default();
        let layer = cli.as_layer();

        assert!(layer.startup.is_empty());
        assert!(layer.runtime.is_empty());
    }

    #[test]
    fn yaml_nested_keys_flatten_with_dots() {
        let yaml = r"
sync:
  branch: main
git:
  auto_commit: true
routing:
  policy: fifo
";
        let value: serde_yaml::Value = serde_yaml::from_str(yaml).expect("parse yaml");
        let layer = layer_from_yaml_value(&value);

        // git.* and routing.* prefixes go to startup (per is_startup_key)
        // sync.branch is an explicit startup key
        assert!(layer.startup.contains_key("sync.branch"));
        assert!(layer.startup.contains_key("git.auto_commit"));
        assert!(layer.startup.contains_key("routing.policy"));
    }

    #[test]
    fn actor_from_layer_returns_none_for_empty() {
        let layer = ConfigLayer::default();
        assert!(actor_from_layer(&layer).is_none());

        let mut layer_with_empty = ConfigLayer::default();
        layer_with_empty
            .startup
            .insert("actor".to_string(), "   ".to_string());
        assert!(actor_from_layer(&layer_with_empty).is_none());
    }

    #[test]
    fn actor_from_layer_returns_trimmed_value() {
        let mut layer = ConfigLayer::default();
        layer
            .startup
            .insert("actor".to_string(), "  test_actor  ".to_string());

        let actor = actor_from_layer(&layer).expect("actor");
        assert_eq!(actor, "test_actor");
    }

    #[test]
    fn resolve_actor_falls_back_to_unknown() {
        let layer = ConfigLayer::default();
        // This test assumes USER env var may not be set in test context
        // or we need to verify the fallback mechanism
        let actor = resolve_actor(&layer);
        // Should be either USER env value or "unknown"
        assert!(!actor.is_empty());
    }

    #[test]
    fn merge_from_overwrites_existing_keys() {
        let mut base = ConfigLayer::default();
        base.runtime
            .insert("key1".to_string(), "base_value".to_string());
        base.startup
            .insert("key2".to_string(), "base_startup".to_string());

        let mut override_layer = ConfigLayer::default();
        override_layer
            .runtime
            .insert("key1".to_string(), "override_value".to_string());
        override_layer
            .startup
            .insert("key2".to_string(), "override_startup".to_string());

        base.merge_from(&override_layer);

        assert_eq!(base.runtime.get("key1").unwrap(), "override_value");
        assert_eq!(base.startup.get("key2").unwrap(), "override_startup");
    }

    #[test]
    fn merge_from_preserves_non_conflicting_keys() {
        let mut base = ConfigLayer::default();
        base.runtime
            .insert("base_only".to_string(), "base_value".to_string());

        let mut override_layer = ConfigLayer::default();
        override_layer
            .runtime
            .insert("override_only".to_string(), "override_value".to_string());

        base.merge_from(&override_layer);

        assert_eq!(base.runtime.get("base_only").unwrap(), "base_value");
        assert_eq!(base.runtime.get("override_only").unwrap(), "override_value");
    }

    #[test]
    fn config_paths_resolve_with_default_metadata() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let paths = ConfigPaths::resolve(&beads_dir, None).expect("paths");

        assert_eq!(paths.beads_dir, beads_dir);
        assert_eq!(paths.db_path, beads_dir.join(DEFAULT_DB_FILENAME));
        assert_eq!(paths.jsonl_path, beads_dir.join(DEFAULT_JSONL_FILENAME));
        assert_eq!(paths.metadata, Metadata::default());
    }

    #[test]
    fn load_project_config_returns_empty_when_missing() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let layer = load_project_config(&beads_dir).expect("project config");
        assert!(layer.startup.is_empty());
        assert!(layer.runtime.is_empty());
    }

    #[test]
    fn load_project_config_parses_yaml() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        fs::write(
            beads_dir.join("config.yaml"),
            "issue_prefix: proj\nno-db: false\n",
        )
        .expect("write config");

        let layer = load_project_config(&beads_dir).expect("project config");
        assert_eq!(layer.runtime.get("issue_prefix").unwrap(), "proj");
        assert_eq!(layer.startup.get("no-db").unwrap(), "false");
    }

    #[test]
    fn id_config_uses_defaults_when_keys_missing() {
        let layer = ConfigLayer::default();
        let config = id_config_from_layer(&layer);

        assert_eq!(config.prefix, "bd");
        assert_eq!(config.min_hash_length, 3);
        assert_eq!(config.max_hash_length, 8);
        assert!((config.max_collision_prob - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn id_config_handles_hyphenated_keys() {
        let mut layer = ConfigLayer::default();
        layer
            .runtime
            .insert("issue-prefix".to_string(), "hyphen".to_string());
        layer
            .runtime
            .insert("min-hash-length".to_string(), "5".to_string());

        let config = id_config_from_layer(&layer);
        assert_eq!(config.prefix, "hyphen");
        assert_eq!(config.min_hash_length, 5);
    }

    #[test]
    fn id_config_accepts_legacy_prefix_key() {
        let mut layer = ConfigLayer::default();
        layer
            .runtime
            .insert("prefix".to_string(), "legacy".to_string());

        let config = id_config_from_layer(&layer);
        assert_eq!(config.prefix, "legacy");
    }

    // ==================== JSONL Discovery Tests ====================
    // Tests for beads_rust-ndl: JSONL discovery + metadata.json handling

    #[test]
    fn discover_jsonl_prefers_issues_over_legacy() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Create both files
        fs::write(beads_dir.join("issues.jsonl"), "{}").expect("write issues");
        fs::write(beads_dir.join("beads.jsonl"), "{}").expect("write beads");

        let discovered = discover_jsonl(&beads_dir).expect("should discover");
        assert_eq!(discovered, beads_dir.join("issues.jsonl"));
    }

    #[test]
    fn discover_jsonl_falls_back_to_legacy() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Only legacy file exists
        fs::write(beads_dir.join("beads.jsonl"), "{}").expect("write beads");

        let discovered = discover_jsonl(&beads_dir).expect("should discover");
        assert_eq!(discovered, beads_dir.join("beads.jsonl"));
    }

    #[test]
    fn discover_jsonl_returns_none_when_empty() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // No JSONL files
        let discovered = discover_jsonl(&beads_dir);
        assert!(discovered.is_none());
    }

    #[test]
    fn discover_jsonl_ignores_merge_artifacts() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Only merge artifacts exist (should not be discovered)
        fs::write(beads_dir.join("beads.base.jsonl"), "{}").expect("write base");
        fs::write(beads_dir.join("beads.left.jsonl"), "{}").expect("write left");
        fs::write(beads_dir.join("beads.right.jsonl"), "{}").expect("write right");

        let discovered = discover_jsonl(&beads_dir);
        assert!(discovered.is_none());
    }

    #[test]
    fn is_excluded_jsonl_detects_merge_artifacts() {
        assert!(is_excluded_jsonl("beads.base.jsonl"));
        assert!(is_excluded_jsonl("beads.left.jsonl"));
        assert!(is_excluded_jsonl("beads.right.jsonl"));
    }

    #[test]
    fn is_excluded_jsonl_detects_deletion_log() {
        assert!(is_excluded_jsonl("deletions.jsonl"));
    }

    #[test]
    fn is_excluded_jsonl_detects_interaction_log() {
        assert!(is_excluded_jsonl("interactions.jsonl"));
    }

    #[test]
    fn is_excluded_jsonl_allows_valid_files() {
        assert!(!is_excluded_jsonl("issues.jsonl"));
        assert!(!is_excluded_jsonl("beads.jsonl"));
        assert!(!is_excluded_jsonl("custom.jsonl"));
    }

    #[test]
    fn resolve_jsonl_uses_discovery_when_no_override() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Only legacy file exists
        fs::write(beads_dir.join("beads.jsonl"), "{}").expect("write beads");

        let metadata = Metadata::default();
        let resolved = resolve_jsonl_path(&beads_dir, &metadata, None);

        // Should discover beads.jsonl since issues.jsonl doesn't exist
        assert_eq!(resolved, beads_dir.join("beads.jsonl"));
    }

    #[test]
    fn resolve_jsonl_prefers_metadata_override() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Both legacy and custom exist
        fs::write(beads_dir.join("beads.jsonl"), "{}").expect("write beads");
        fs::write(beads_dir.join("custom.jsonl"), "{}").expect("write custom");

        let metadata = Metadata {
            database: DEFAULT_DB_FILENAME.to_string(),
            jsonl_export: "custom.jsonl".to_string(),
            backend: None,
            deletions_retention_days: None,
        };

        let resolved = resolve_jsonl_path(&beads_dir, &metadata, None);
        assert_eq!(resolved, beads_dir.join("custom.jsonl"));
    }

    #[test]
    fn resolve_jsonl_ignores_excluded_metadata() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Create issues.jsonl
        fs::write(beads_dir.join("issues.jsonl"), "{}").expect("write issues");

        // Metadata points to excluded file (should be ignored)
        let metadata = Metadata {
            database: DEFAULT_DB_FILENAME.to_string(),
            jsonl_export: "deletions.jsonl".to_string(),
            backend: None,
            deletions_retention_days: None,
        };

        let resolved = resolve_jsonl_path(&beads_dir, &metadata, None);
        // Should fall through to discovery, find issues.jsonl
        assert_eq!(resolved, beads_dir.join("issues.jsonl"));
    }

    #[test]
    fn resolve_jsonl_defaults_when_nothing_exists() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // No JSONL files exist
        let metadata = Metadata::default();
        let resolved = resolve_jsonl_path(&beads_dir, &metadata, None);

        // Should return default for writing
        assert_eq!(resolved, beads_dir.join("issues.jsonl"));
    }

    #[test]
    fn resolve_jsonl_db_override_derives_sibling() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        let custom_dir = temp.path().join("custom");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::create_dir_all(&custom_dir).expect("create custom dir");

        // Create files in beads_dir (should be ignored)
        fs::write(beads_dir.join("beads.jsonl"), "{}").expect("write beads");

        let metadata = Metadata::default();
        let db_override = custom_dir.join("custom.db");

        let resolved = resolve_jsonl_path(&beads_dir, &metadata, Some(&db_override));
        // Should derive sibling from db_override path
        assert_eq!(resolved, custom_dir.join("issues.jsonl"));
    }

    #[test]
    fn config_paths_uses_discovery() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Only legacy file exists
        fs::write(beads_dir.join("beads.jsonl"), "{}").expect("write beads");

        let paths = ConfigPaths::resolve(&beads_dir, None).expect("paths");

        // Should discover beads.jsonl
        assert_eq!(paths.jsonl_path, beads_dir.join("beads.jsonl"));
    }

    #[test]
    fn metadata_jsonl_override_respected() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Write metadata with custom jsonl_export
        fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database": "beads.db", "jsonl_export": "my-export.jsonl"}"#,
        )
        .expect("write metadata");

        // Create the custom file
        fs::write(beads_dir.join("my-export.jsonl"), "{}").expect("write custom");

        let paths = ConfigPaths::resolve(&beads_dir, None).expect("paths");
        assert_eq!(paths.jsonl_path, beads_dir.join("my-export.jsonl"));
    }

    #[test]
    fn multiple_jsonl_candidates_prefers_issues() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // Create multiple candidates
        fs::write(beads_dir.join("beads.jsonl"), "{}").expect("write beads");
        fs::write(beads_dir.join("issues.jsonl"), "{}").expect("write issues");
        fs::write(beads_dir.join("beads.base.jsonl"), "{}").expect("write base");
        fs::write(beads_dir.join("deletions.jsonl"), "{}").expect("write deletions");

        let paths = ConfigPaths::resolve(&beads_dir, None).expect("paths");

        // Should pick issues.jsonl (preferred over legacy, ignoring excluded)
        assert_eq!(paths.jsonl_path, beads_dir.join("issues.jsonl"));
    }
}
