#![allow(clippy::all, clippy::pedantic, clippy::nursery, dead_code)]
//! Schema Conformance Tests: Validate br (Rust) and bd (Go) have identical DB structure
//!
//! This harness compares:
//! - SQLite schema (tables, columns, types, constraints, indexes)
//! - metadata.json structure and defaults
//! - JSONL field presence and serialization
//!
//! Uses PRAGMA introspection to avoid .schema formatting differences.

mod common;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tracing::info;

/// Output from running a command
#[derive(Debug)]
pub struct CmdOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub duration: Duration,
}

/// Schema workspace for paired br/bd comparisons
pub struct SchemaWorkspace {
    pub temp_dir: TempDir,
    pub br_root: PathBuf,
    pub bd_root: PathBuf,
}

impl SchemaWorkspace {
    pub fn new() -> Self {
        let temp_dir = TempDir::new().expect("create temp dir");
        let root = temp_dir.path().to_path_buf();
        let br_root = root.join("br_workspace");
        let bd_root = root.join("bd_workspace");

        fs::create_dir_all(&br_root).expect("create br workspace");
        fs::create_dir_all(&bd_root).expect("create bd workspace");

        Self {
            temp_dir,
            br_root,
            bd_root,
        }
    }

    /// Initialize both workspaces
    pub fn init_both(&self) {
        let br_out = self.run_br(&["init"]);
        assert!(br_out.success, "br init failed: {}", br_out.stderr);

        let bd_out = self.run_bd(&["init"]);
        assert!(bd_out.success, "bd init failed: {}", bd_out.stderr);
    }

    /// Run br command
    pub fn run_br(&self, args: &[&str]) -> CmdOutput {
        run_binary("br", &self.br_root, args)
    }

    /// Run bd command
    pub fn run_bd(&self, args: &[&str]) -> CmdOutput {
        run_binary("bd", &self.bd_root, args)
    }

    /// Get path to br database
    pub fn br_db_path(&self) -> PathBuf {
        self.br_root.join(".beads").join("beads.db")
    }

    /// Get path to bd database
    pub fn bd_db_path(&self) -> PathBuf {
        self.bd_root.join(".beads").join("beads.db")
    }

    /// Get path to br metadata.json
    pub fn br_metadata_path(&self) -> PathBuf {
        self.br_root.join(".beads").join("metadata.json")
    }

    /// Get path to bd metadata.json
    pub fn bd_metadata_path(&self) -> PathBuf {
        self.bd_root.join(".beads").join("metadata.json")
    }

    /// Get path to br JSONL export
    pub fn br_jsonl_path(&self) -> PathBuf {
        self.br_root.join(".beads").join("issues.jsonl")
    }

    /// Get path to bd JSONL export
    pub fn bd_jsonl_path(&self) -> PathBuf {
        self.bd_root.join(".beads").join("issues.jsonl")
    }

    /// Run SQLite PRAGMA command and return results
    pub fn run_sqlite_pragma(&self, db_path: &PathBuf, pragma: &str) -> String {
        let output = Command::new("sqlite3")
            .args([db_path.to_str().unwrap(), pragma])
            .output()
            .expect("run sqlite3");
        String::from_utf8_lossy(&output.stdout).to_string()
    }
}

/// Check if both br (release binary) and bd are available for schema comparison tests
fn binaries_available() -> bool {
    // Check br release binary exists
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let manifest_dir =
                std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(manifest_dir).join("target")
        });
    let br_path = target_dir.join("release").join("br");
    let br_exists = br_path.exists();

    // Check bd is available
    let bd_available = Command::new("bd")
        .arg("version")
        .output()
        .is_ok_and(|o| o.status.success());

    br_exists && bd_available
}

/// Skip test if br release binary or bd is not available (used in CI)
macro_rules! skip_if_no_binaries {
    () => {
        if !binaries_available() {
            eprintln!("Skipping test: 'br' release binary or 'bd' not found (expected in CI)");
            return;
        }
    };
}

fn run_binary(binary: &str, cwd: &PathBuf, args: &[&str]) -> CmdOutput {
    let cmd_path = if binary == "br" {
        // Use cargo-built binary, respecting CARGO_TARGET_DIR if set
        let target_dir = std::env::var("CARGO_TARGET_DIR")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let manifest_dir =
                    std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
                PathBuf::from(manifest_dir).join("target")
            });
        target_dir.join("release").join("br")
    } else {
        // Use system bd
        PathBuf::from(binary)
    };

    let start = Instant::now();
    let output = Command::new(&cmd_path)
        .current_dir(cwd)
        .args(args)
        .env("NO_COLOR", "1")
        .env("HOME", cwd)
        .output()
        .expect(&format!("run {binary}"));
    let duration = start.elapsed();

    CmdOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        success: output.status.success(),
        duration,
    }
}

/// Column information from PRAGMA table_info
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct ColumnInfo {
    pub cid: i32,
    pub name: String,
    pub col_type: String,
    pub notnull: bool,
    pub dflt_value: Option<String>,
    pub pk: bool,
}

impl ColumnInfo {
    /// Parse from PRAGMA table_info output line
    /// Format: cid|name|type|notnull|dflt_value|pk
    pub fn from_pragma_line(line: &str) -> Option<Self> {
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() < 6 {
            return None;
        }
        Some(Self {
            cid: parts[0].parse().unwrap_or(0),
            name: parts[1].to_string(),
            col_type: parts[2].to_string(),
            notnull: parts[3] == "1",
            dflt_value: if parts[4].is_empty() {
                None
            } else {
                Some(parts[4].to_string())
            },
            pk: parts[5] != "0",
        })
    }
}

/// Index information from PRAGMA index_list + index_info
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct IndexInfo {
    pub name: String,
    pub unique: bool,
    pub columns: Vec<String>,
}

/// Schema comparison result
#[derive(Debug, Default)]
pub struct SchemaComparison {
    pub tables_br_only: Vec<String>,
    pub tables_bd_only: Vec<String>,
    pub tables_common: Vec<String>,
    pub column_diffs: Vec<ColumnDiff>,
    pub index_diffs: Vec<IndexDiff>,
}

#[derive(Debug)]
pub struct ColumnDiff {
    pub table: String,
    pub column: String,
    pub diff_type: String,
    pub br_value: String,
    pub bd_value: String,
}

#[derive(Debug)]
pub struct IndexDiff {
    pub table: String,
    pub index: String,
    pub diff_type: String,
    pub br_value: String,
    pub bd_value: String,
}

impl SchemaComparison {
    pub fn is_compatible(&self) -> bool {
        // Allow br to have additional tables (internal use)
        // but bd should not have tables br lacks (feature parity)
        self.tables_bd_only.is_empty() && self.column_diffs.is_empty()
    }

    pub fn summary(&self) -> String {
        let mut lines = Vec::new();
        if !self.tables_br_only.is_empty() {
            lines.push(format!("Tables in br only: {:?}", self.tables_br_only));
        }
        if !self.tables_bd_only.is_empty() {
            lines.push(format!("Tables in bd only: {:?}", self.tables_bd_only));
        }
        for diff in &self.column_diffs {
            lines.push(format!(
                "Column diff in {}.{}: {} (br: {}, bd: {})",
                diff.table, diff.column, diff.diff_type, diff.br_value, diff.bd_value
            ));
        }
        for diff in &self.index_diffs {
            lines.push(format!(
                "Index diff in {}.{}: {} (br: {}, bd: {})",
                diff.table, diff.index, diff.diff_type, diff.br_value, diff.bd_value
            ));
        }
        if lines.is_empty() {
            "Schema compatible".to_string()
        } else {
            lines.join("\n")
        }
    }
}

/// Extract table list from SQLite database
fn get_table_list(workspace: &SchemaWorkspace, db_path: &PathBuf) -> Vec<String> {
    let output = workspace.run_sqlite_pragma(
        db_path,
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name;",
    );
    output
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.trim().to_string())
        .collect()
}

/// Extract column info for a table
fn get_table_columns(
    workspace: &SchemaWorkspace,
    db_path: &PathBuf,
    table: &str,
) -> Vec<ColumnInfo> {
    let output = workspace.run_sqlite_pragma(db_path, &format!("PRAGMA table_info({table});"));
    output
        .lines()
        .filter_map(ColumnInfo::from_pragma_line)
        .collect()
}

/// Extract index info for a table
fn get_table_indexes(
    workspace: &SchemaWorkspace,
    db_path: &PathBuf,
    table: &str,
) -> Vec<IndexInfo> {
    let list_output = workspace.run_sqlite_pragma(db_path, &format!("PRAGMA index_list({table});"));

    let mut indexes = Vec::new();
    for line in list_output.lines() {
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() < 3 {
            continue;
        }
        let index_name = parts[1];
        let unique = parts[2] == "1";

        // Get columns for this index
        let info_output =
            workspace.run_sqlite_pragma(db_path, &format!("PRAGMA index_info({index_name});"));
        let columns: Vec<String> = info_output
            .lines()
            .filter_map(|l| {
                let cols: Vec<&str> = l.split('|').collect();
                if cols.len() >= 3 {
                    Some(cols[2].to_string())
                } else {
                    None
                }
            })
            .collect();

        indexes.push(IndexInfo {
            name: index_name.to_string(),
            unique,
            columns,
        });
    }
    indexes
}

/// Compare schemas between br and bd databases
fn compare_schemas(workspace: &SchemaWorkspace) -> SchemaComparison {
    let br_db = workspace.br_db_path();
    let bd_db = workspace.bd_db_path();

    let br_tables: BTreeSet<String> = get_table_list(workspace, &br_db).into_iter().collect();
    let bd_tables: BTreeSet<String> = get_table_list(workspace, &bd_db).into_iter().collect();

    let mut result = SchemaComparison::default();

    // Find table differences
    result.tables_br_only = br_tables.difference(&bd_tables).cloned().collect();
    result.tables_bd_only = bd_tables.difference(&br_tables).cloned().collect();
    result.tables_common = br_tables.intersection(&bd_tables).cloned().collect();

    // Compare columns in common tables
    for table in &result.tables_common {
        // Skip internal SQLite tables
        if table.starts_with("sqlite_") {
            continue;
        }

        let br_columns = get_table_columns(workspace, &br_db, table);
        let bd_columns = get_table_columns(workspace, &bd_db, table);

        let br_col_map: BTreeMap<String, ColumnInfo> = br_columns
            .into_iter()
            .map(|c| (c.name.clone(), c))
            .collect();
        let bd_col_map: BTreeMap<String, ColumnInfo> = bd_columns
            .into_iter()
            .map(|c| (c.name.clone(), c))
            .collect();

        let br_names: BTreeSet<String> = br_col_map.keys().cloned().collect();
        let bd_names: BTreeSet<String> = bd_col_map.keys().cloned().collect();

        // Columns only in br
        for name in br_names.difference(&bd_names) {
            result.column_diffs.push(ColumnDiff {
                table: table.clone(),
                column: name.clone(),
                diff_type: "missing_in_bd".to_string(),
                br_value: format!("{:?}", br_col_map.get(name)),
                bd_value: "None".to_string(),
            });
        }

        // Columns only in bd
        for name in bd_names.difference(&br_names) {
            result.column_diffs.push(ColumnDiff {
                table: table.clone(),
                column: name.clone(),
                diff_type: "missing_in_br".to_string(),
                br_value: "None".to_string(),
                bd_value: format!("{:?}", bd_col_map.get(name)),
            });
        }

        // Compare common columns (type mismatch is important)
        for name in br_names.intersection(&bd_names) {
            let br_col = br_col_map.get(name).unwrap();
            let bd_col = bd_col_map.get(name).unwrap();

            // Normalize types for comparison (SQLite is flexible with type names)
            let br_type = normalize_sqlite_type(&br_col.col_type);
            let bd_type = normalize_sqlite_type(&bd_col.col_type);

            if br_type != bd_type {
                result.column_diffs.push(ColumnDiff {
                    table: table.clone(),
                    column: name.clone(),
                    diff_type: "type_mismatch".to_string(),
                    br_value: br_col.col_type.clone(),
                    bd_value: bd_col.col_type.clone(),
                });
            }

            // Check NOT NULL constraint
            if br_col.notnull != bd_col.notnull {
                result.column_diffs.push(ColumnDiff {
                    table: table.clone(),
                    column: name.clone(),
                    diff_type: "notnull_mismatch".to_string(),
                    br_value: br_col.notnull.to_string(),
                    bd_value: bd_col.notnull.to_string(),
                });
            }

            // Check PRIMARY KEY
            if br_col.pk != bd_col.pk {
                result.column_diffs.push(ColumnDiff {
                    table: table.clone(),
                    column: name.clone(),
                    diff_type: "pk_mismatch".to_string(),
                    br_value: br_col.pk.to_string(),
                    bd_value: bd_col.pk.to_string(),
                });
            }
        }

        // Compare indexes
        let br_indexes = get_table_indexes(workspace, &br_db, table);
        let bd_indexes = get_table_indexes(workspace, &bd_db, table);

        let br_idx_map: BTreeMap<String, IndexInfo> = br_indexes
            .into_iter()
            .map(|i| (i.name.clone(), i))
            .collect();
        let bd_idx_map: BTreeMap<String, IndexInfo> = bd_indexes
            .into_iter()
            .map(|i| (i.name.clone(), i))
            .collect();

        // Note: Index name differences are expected (auto-generated names differ)
        // Compare by column set instead
        let br_idx_by_cols: BTreeSet<Vec<String>> =
            br_idx_map.values().map(|i| i.columns.clone()).collect();
        let bd_idx_by_cols: BTreeSet<Vec<String>> =
            bd_idx_map.values().map(|i| i.columns.clone()).collect();

        for cols in br_idx_by_cols.difference(&bd_idx_by_cols) {
            result.index_diffs.push(IndexDiff {
                table: table.clone(),
                index: cols.join(","),
                diff_type: "index_missing_in_bd".to_string(),
                br_value: format!("{:?}", cols),
                bd_value: "None".to_string(),
            });
        }

        for cols in bd_idx_by_cols.difference(&br_idx_by_cols) {
            result.index_diffs.push(IndexDiff {
                table: table.clone(),
                index: cols.join(","),
                diff_type: "index_missing_in_br".to_string(),
                br_value: "None".to_string(),
                bd_value: format!("{:?}", cols),
            });
        }
    }

    result
}

/// Normalize SQLite type for comparison
fn normalize_sqlite_type(t: &str) -> String {
    let upper = t.to_uppercase();
    match upper.as_str() {
        "INTEGER" | "INT" | "BIGINT" | "SMALLINT" | "TINYINT" => "INTEGER".to_string(),
        "TEXT" | "VARCHAR" | "CHAR" | "CLOB" => "TEXT".to_string(),
        "REAL" | "DOUBLE" | "FLOAT" => "REAL".to_string(),
        "BLOB" => "BLOB".to_string(),
        "BOOLEAN" | "BOOL" => "INTEGER".to_string(), // SQLite stores bools as integers
        "" => "".to_string(),
        _ => upper,
    }
}

/// Compare metadata.json between br and bd
fn compare_metadata(workspace: &SchemaWorkspace) -> Result<(), String> {
    let br_meta = workspace.br_metadata_path();
    let bd_meta = workspace.bd_metadata_path();

    let br_content = fs::read_to_string(&br_meta)
        .map_err(|e| format!("Failed to read br metadata.json: {e}"))?;
    let bd_content = fs::read_to_string(&bd_meta)
        .map_err(|e| format!("Failed to read bd metadata.json: {e}"))?;

    let br_json: Value =
        serde_json::from_str(&br_content).map_err(|e| format!("br metadata.json invalid: {e}"))?;
    let bd_json: Value =
        serde_json::from_str(&bd_content).map_err(|e| format!("bd metadata.json invalid: {e}"))?;

    // Compare required fields
    let required_fields = ["database", "jsonl_export"];
    for field in required_fields {
        let br_val = br_json.get(field);
        let bd_val = bd_json.get(field);

        if br_val.is_none() && bd_val.is_some() {
            return Err(format!(
                "metadata.json field '{}' missing in br but present in bd",
                field
            ));
        }
        if br_val.is_some() && bd_val.is_none() {
            // br may have additional fields, that's acceptable
            continue;
        }
        if br_val != bd_val {
            return Err(format!(
                "metadata.json field '{}' differs: br={:?}, bd={:?}",
                field, br_val, bd_val
            ));
        }
    }

    // Check that bd has no fields br doesn't know about
    if let (Value::Object(br_obj), Value::Object(bd_obj)) = (&br_json, &bd_json) {
        let br_keys: HashSet<&String> = br_obj.keys().collect();
        let bd_keys: HashSet<&String> = bd_obj.keys().collect();

        let bd_only: Vec<&&String> = bd_keys.difference(&br_keys).collect();
        if !bd_only.is_empty() {
            return Err(format!(
                "metadata.json has fields in bd not in br: {:?}",
                bd_only
            ));
        }
    }

    Ok(())
}

/// JSONL fields that bd always serializes but br skips when empty.
/// This is acceptable - br uses skip_serializing_if = "is_empty" for these.
const KNOWN_JSONL_BD_ONLY_FIELDS: &[&str] = &[
    "created_by", // br skips when empty, bd always includes
                  // Add more fields here as needed
];

/// Compare JSONL field presence for an issue
fn compare_jsonl_fields(workspace: &SchemaWorkspace) -> Result<(), String> {
    let br_jsonl = workspace.br_jsonl_path();
    let bd_jsonl = workspace.bd_jsonl_path();

    // Read first line from each (they should have at least one issue after creation)
    let br_content = fs::read_to_string(&br_jsonl)
        .map_err(|e| format!("Failed to read br issues.jsonl: {e}"))?;
    let bd_content = fs::read_to_string(&bd_jsonl)
        .map_err(|e| format!("Failed to read bd issues.jsonl: {e}"))?;

    let br_line = br_content
        .lines()
        .next()
        .ok_or("br issues.jsonl is empty")?;
    let bd_line = bd_content
        .lines()
        .next()
        .ok_or("bd issues.jsonl is empty")?;

    let br_issue: Value =
        serde_json::from_str(br_line).map_err(|e| format!("br JSONL line invalid: {e}"))?;
    let bd_issue: Value =
        serde_json::from_str(bd_line).map_err(|e| format!("bd JSONL line invalid: {e}"))?;

    // Compare field presence (not values - those can differ)
    if let (Value::Object(br_obj), Value::Object(bd_obj)) = (&br_issue, &bd_issue) {
        let br_keys: BTreeSet<&String> = br_obj.keys().collect();
        let bd_keys: BTreeSet<&String> = bd_obj.keys().collect();

        // Fields only in br (acceptable - we may add fields)
        let br_only: Vec<&&String> = br_keys.difference(&bd_keys).collect();
        if !br_only.is_empty() {
            info!("JSONL fields in br but not bd: {:?}", br_only);
        }

        // Fields only in bd (filter out known differences)
        let bd_only: Vec<&&String> = bd_keys.difference(&br_keys).collect();
        let unexpected_bd_only: Vec<&&&String> = bd_only
            .iter()
            .filter(|f| !KNOWN_JSONL_BD_ONLY_FIELDS.contains(&f.as_str()))
            .collect();

        // Log known differences for visibility
        for field in &bd_only {
            if KNOWN_JSONL_BD_ONLY_FIELDS.contains(&field.as_str()) {
                info!(
                    "Known JSONL difference: field '{}' in bd but not br (skip_serializing_if)",
                    field
                );
            }
        }

        if !unexpected_bd_only.is_empty() {
            return Err(format!(
                "JSONL fields in bd but not br (feature parity issue): {:?}",
                unexpected_bd_only
            ));
        }

        // Check value type compatibility for common fields
        for key in br_keys.intersection(&bd_keys) {
            let br_val = br_obj.get(*key).unwrap();
            let bd_val = bd_obj.get(*key).unwrap();

            // Check structural compatibility (same type or both null-ish)
            if !values_structurally_compatible(br_val, bd_val) {
                return Err(format!(
                    "JSONL field '{}' has incompatible types: br={}, bd={}",
                    key,
                    value_type_name(br_val),
                    value_type_name(bd_val)
                ));
            }
        }
    }

    Ok(())
}

fn values_structurally_compatible(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => true, // null is compatible with anything (optional)
        (Value::Bool(_), Value::Bool(_)) => true,
        (Value::Number(_), Value::Number(_)) => true,
        (Value::String(_), Value::String(_)) => true,
        (Value::Array(_), Value::Array(_)) => true,
        (Value::Object(_), Value::Object(_)) => true,
        _ => false,
    }
}

fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

// ============================================================================
// TESTS
// ============================================================================

/// Tables that bd has but br intentionally doesn't implement (yet)
/// These are tracked as known differences for documentation purposes.
/// When br implements these, remove them from this list.
const KNOWN_BD_ONLY_TABLES: &[&str] = &[
    "compaction_snapshots", // Used for compaction history tracking
    "issue_snapshots",      // Used for issue history/versioning
    "repo_mtimes",          // Used for repo modification time tracking
];

/// Columns in the issues table that bd has but br intentionally doesn't implement.
/// These are Gastown/advanced features excluded from the br port per AGENTS.md.
/// When br implements these, remove them from this list.
const KNOWN_BD_ONLY_COLUMNS: &[&str] = &[
    // Gastown/agent features in bd that br hasn't ported
    "actor",           // Actor reference for agent coordination
    "crystallizes",    // Crystallization flag
    "event_kind",      // Event kind classification
    "gate_status",     // Gate/workflow status
    "hook_name",       // Hook automation
    "mol_type",        // Molecule type classification
    "molecule_id",     // Molecule grouping
    "payload",         // Payload data
    "priority_origin", // Priority inheritance tracking
    "quality_score",   // Quality scoring
    "retry_count",     // Retry automation
    "target",          // Target system reference
    "work_type",       // Work type classification (mutex, etc.)
];

/// Columns in the issues table that br has but bd doesn't have.
/// These are features br has implemented ahead of bd or internal to br.
/// This is acceptable as br can be a superset of bd's schema.
const KNOWN_BR_ONLY_COLUMNS: &[&str] = &[
    // Gastown/agent features br has implemented
    "agent_state",     // Agent state tracking
    "await_id",        // Await coordination
    "await_type",      // Await type classification
    "ephemeral",       // Ephemeral issues (agent coordination)
    "hook_bead",       // Hook bead reference
    "is_template",     // Template issues
    "last_activity",   // Last activity tracking
    "pinned",          // Pinning support
    "rig",             // Rig reference
    "role_bead",       // Role bead reference
    "role_type",       // Role-based typing
    "source_repo",     // Multi-repo source tracking
    "timeout_ns",      // Timeout configuration
    "waiters",         // Waiter list for coordination
];

/// Known type differences between br and bd that are acceptable.
/// SQLite is flexible with types; these differences don't affect functionality.
const KNOWN_TYPE_DIFFERENCES: &[&str] = &[
    // br uses TEXT for timestamps (ISO8601 strings), bd uses DATETIME (still TEXT internally)
    "closed_at",
    "compacted_at",
    "created_at",
    "defer_until",
    "deleted_at",
    "due_at",
    "updated_at",
];

/// Known NOT NULL differences that are acceptable.
/// br is stricter with NOT NULL (empty string default), bd allows NULL.
const KNOWN_NOTNULL_DIFFERENCES: &[&str] = &[
    "close_reason",
    "closed_by_session",
    "created_by",
    "delete_reason",
    "deleted_by",
    "original_type",
    "owner",
    "sender",
    "source_system",
];

#[test]
fn conformance_schema_tables_present() {
    skip_if_no_binaries!();
    common::init_test_logging();
    info!("Starting conformance_schema_tables_present test");

    let workspace = SchemaWorkspace::new();
    workspace.init_both();

    let comparison = compare_schemas(&workspace);

    // Log findings
    info!("Schema comparison: {}", comparison.summary());
    info!("Tables in br only: {:?}", comparison.tables_br_only);
    info!("Tables in bd only: {:?}", comparison.tables_bd_only);

    // Check for unexpected bd-only tables (not in our known list)
    let unexpected_bd_tables: Vec<&String> = comparison
        .tables_bd_only
        .iter()
        .filter(|t| !KNOWN_BD_ONLY_TABLES.contains(&t.as_str()))
        .collect();

    assert!(
        unexpected_bd_tables.is_empty(),
        "bd has unexpected tables not in br (not in known list): {:?}\nKnown differences: {:?}",
        unexpected_bd_tables,
        KNOWN_BD_ONLY_TABLES
    );

    // Document known differences for visibility
    for known in KNOWN_BD_ONLY_TABLES {
        if comparison.tables_bd_only.contains(&known.to_string()) {
            info!(
                "Known schema difference: table '{}' in bd but not br (tracked)",
                known
            );
        }
    }

    // Required tables must be in both
    let required_tables = [
        "issues",
        "dependencies",
        "labels",
        "comments",
        "events",
        "config",
        "metadata",
    ];

    for table in required_tables {
        assert!(
            comparison.tables_common.contains(&table.to_string()),
            "Required table '{}' not in both br and bd",
            table
        );
    }
}

#[test]
fn conformance_schema_issues_columns() {
    skip_if_no_binaries!();
    common::init_test_logging();
    info!("Starting conformance_schema_issues_columns test");

    let workspace = SchemaWorkspace::new();
    workspace.init_both();

    let br_db = workspace.br_db_path();
    let bd_db = workspace.bd_db_path();

    let br_cols = get_table_columns(&workspace, &br_db, "issues");
    let bd_cols = get_table_columns(&workspace, &bd_db, "issues");

    // Core columns that must match
    let core_columns = [
        "id",
        "title",
        "description",
        "status",
        "priority",
        "issue_type",
        "assignee",
        "owner",
        "created_at",
        "updated_at",
        "closed_at",
    ];

    let br_names: HashSet<String> = br_cols.iter().map(|c| c.name.clone()).collect();
    let bd_names: HashSet<String> = bd_cols.iter().map(|c| c.name.clone()).collect();

    for col in core_columns {
        assert!(
            br_names.contains(col),
            "Core column '{}' missing in br issues table",
            col
        );
        assert!(
            bd_names.contains(col),
            "Core column '{}' missing in bd issues table",
            col
        );
    }

    // bd should not have unexpected columns br doesn't have (feature parity)
    // Filter out known bd-only columns (Gastown features intentionally not ported)
    let bd_only: Vec<&String> = bd_names.difference(&br_names).collect();
    let unexpected_bd_cols: Vec<&&String> = bd_only
        .iter()
        .filter(|c| !KNOWN_BD_ONLY_COLUMNS.contains(&c.as_str()))
        .collect();

    // Log known differences for visibility
    for col in &bd_only {
        if KNOWN_BD_ONLY_COLUMNS.contains(&col.as_str()) {
            info!(
                "Known schema difference: column '{}' in bd but not br (Gastown feature)",
                col
            );
        }
    }

    assert!(
        unexpected_bd_cols.is_empty(),
        "bd issues table has unexpected columns not in br (not in known list): {:?}\nKnown differences: {:?}",
        unexpected_bd_cols,
        KNOWN_BD_ONLY_COLUMNS
    );
}

#[test]
fn conformance_schema_dependencies_structure() {
    skip_if_no_binaries!();
    common::init_test_logging();
    info!("Starting conformance_schema_dependencies_structure test");

    let workspace = SchemaWorkspace::new();
    workspace.init_both();

    let br_db = workspace.br_db_path();
    let bd_db = workspace.bd_db_path();

    let br_cols = get_table_columns(&workspace, &br_db, "dependencies");
    let bd_cols = get_table_columns(&workspace, &bd_db, "dependencies");

    // Required columns for dependencies
    let required = ["issue_id", "depends_on_id", "type", "created_at"];

    let br_names: HashSet<String> = br_cols.iter().map(|c| c.name.clone()).collect();
    let bd_names: HashSet<String> = bd_cols.iter().map(|c| c.name.clone()).collect();

    for col in required {
        assert!(
            br_names.contains(col),
            "Column '{}' missing in br dependencies table",
            col
        );
        assert!(
            bd_names.contains(col),
            "Column '{}' missing in bd dependencies table",
            col
        );
    }
}

#[test]
fn conformance_schema_labels_comments() {
    skip_if_no_binaries!();
    common::init_test_logging();
    info!("Starting conformance_schema_labels_comments test");

    let workspace = SchemaWorkspace::new();
    workspace.init_both();

    let br_db = workspace.br_db_path();
    let bd_db = workspace.bd_db_path();

    // Check labels table
    let br_labels = get_table_columns(&workspace, &br_db, "labels");
    let bd_labels = get_table_columns(&workspace, &bd_db, "labels");

    let br_label_names: HashSet<String> = br_labels.iter().map(|c| c.name.clone()).collect();
    let bd_label_names: HashSet<String> = bd_labels.iter().map(|c| c.name.clone()).collect();

    assert!(
        br_label_names.contains("issue_id"),
        "labels.issue_id missing in br"
    );
    assert!(
        br_label_names.contains("label"),
        "labels.label missing in br"
    );
    assert!(
        bd_label_names.contains("issue_id"),
        "labels.issue_id missing in bd"
    );
    assert!(
        bd_label_names.contains("label"),
        "labels.label missing in bd"
    );

    // Check comments table
    let br_comments = get_table_columns(&workspace, &br_db, "comments");
    let bd_comments = get_table_columns(&workspace, &bd_db, "comments");

    let br_comment_names: HashSet<String> = br_comments.iter().map(|c| c.name.clone()).collect();
    let bd_comment_names: HashSet<String> = bd_comments.iter().map(|c| c.name.clone()).collect();

    let comment_required = ["id", "issue_id", "author", "text", "created_at"];
    for col in comment_required {
        assert!(
            br_comment_names.contains(col),
            "comments.{} missing in br",
            col
        );
        assert!(
            bd_comment_names.contains(col),
            "comments.{} missing in bd",
            col
        );
    }
}

#[test]
fn conformance_schema_indexes() {
    skip_if_no_binaries!();
    common::init_test_logging();
    info!("Starting conformance_schema_indexes test");

    let workspace = SchemaWorkspace::new();
    workspace.init_both();

    let comparison = compare_schemas(&workspace);

    // Log index differences (informational, not necessarily failures)
    for diff in &comparison.index_diffs {
        info!(
            "Index diff in {}: {} - {}",
            diff.table, diff.index, diff.diff_type
        );
    }

    // Critical indexes that should exist in both
    let br_db = workspace.br_db_path();
    let bd_db = workspace.bd_db_path();

    let br_issues_idx = get_table_indexes(&workspace, &br_db, "issues");
    let bd_issues_idx = get_table_indexes(&workspace, &bd_db, "issues");

    // Check for status index (commonly used)
    let br_has_status_idx = br_issues_idx
        .iter()
        .any(|i| i.columns.contains(&"status".to_string()));
    let bd_has_status_idx = bd_issues_idx
        .iter()
        .any(|i| i.columns.contains(&"status".to_string()));

    // Both should have a status index for ready/blocked queries
    assert!(
        br_has_status_idx,
        "br should have an index on issues.status"
    );
    assert!(
        bd_has_status_idx,
        "bd should have an index on issues.status"
    );
}

#[test]
fn conformance_metadata_json_structure() {
    skip_if_no_binaries!();
    common::init_test_logging();
    info!("Starting conformance_metadata_json_structure test");

    let workspace = SchemaWorkspace::new();
    workspace.init_both();

    let result = compare_metadata(&workspace);
    assert!(
        result.is_ok(),
        "metadata.json comparison failed: {:?}",
        result.err()
    );
}

#[test]
fn conformance_jsonl_field_parity() {
    skip_if_no_binaries!();
    common::init_test_logging();
    info!("Starting conformance_jsonl_field_parity test");

    let workspace = SchemaWorkspace::new();
    workspace.init_both();

    // Create an issue to have something in JSONL
    let br_create = workspace.run_br(&["create", "Test issue for JSONL parity", "--json"]);
    assert!(br_create.success, "br create failed: {}", br_create.stderr);

    let bd_create = workspace.run_bd(&["create", "Test issue for JSONL parity", "--json"]);
    assert!(bd_create.success, "bd create failed: {}", bd_create.stderr);

    // Force flush to JSONL
    let _ = workspace.run_br(&["sync", "--flush-only"]);
    let _ = workspace.run_bd(&["sync", "--flush-only"]);

    let result = compare_jsonl_fields(&workspace);
    assert!(
        result.is_ok(),
        "JSONL field parity check failed: {:?}",
        result.err()
    );
}

#[test]
fn conformance_jsonl_compaction_level_serialization() {
    skip_if_no_binaries!();
    common::init_test_logging();
    info!("Starting conformance_jsonl_compaction_level_serialization test");

    let workspace = SchemaWorkspace::new();
    workspace.init_both();

    // Create and close an issue to test compaction_level
    let br_create = workspace.run_br(&["create", "Test compaction", "--json"]);
    assert!(br_create.success, "br create failed: {}", br_create.stderr);

    let br_json: Value = serde_json::from_str(&br_create.stdout).unwrap_or_default();
    let br_id = br_json
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    // Close the issue
    let _ = workspace.run_br(&["close", br_id, "--reason", "Testing"]);
    let _ = workspace.run_br(&["sync", "--flush-only"]);

    // Read JSONL and check compaction_level serialization
    let br_jsonl = workspace.br_jsonl_path();
    let content = fs::read_to_string(&br_jsonl).expect("read br jsonl");

    for line in content.lines() {
        if let Ok(issue) = serde_json::from_str::<Value>(line) {
            // compaction_level should be serialized as integer (0 when None)
            if let Some(level) = issue.get("compaction_level") {
                assert!(
                    level.is_number(),
                    "compaction_level should be a number, got: {:?}",
                    level
                );
            }
        }
    }
}

/// Known schema differences in tables other than issues.
/// These are implementation differences between br and bd that are acceptable.
const KNOWN_OTHER_TABLE_DIFFS: &[(&str, &str, &str)] = &[
    // blocked_issues_cache: br has blocked_by_json, bd doesn't
    ("blocked_issues_cache", "blocked_by_json", "missing_in_bd"),
    ("blocked_issues_cache", "issue_id", "notnull_mismatch"),
    // child_counters: different column names for same purpose
    // br uses last_child, bd uses next_child_number
    ("child_counters", "last_child", "missing_in_bd"),
    ("child_counters", "next_child_number", "missing_in_br"),
    // comments: type difference for timestamps
    ("comments", "created_at", "type_mismatch"),
    // dependencies: schema differences
    ("dependencies", "created_at", "type_mismatch"),
    ("dependencies", "created_at", "notnull_mismatch"),
    ("dependencies", "created_by", "notnull_mismatch"),
    ("dependencies", "type", "pk_mismatch"),
    // dirty_issues: schema differences
    ("dirty_issues", "content_hash", "missing_in_br"),
    ("dirty_issues", "marked_at", "type_mismatch"),
    // events: type difference for timestamps
    ("events", "created_at", "type_mismatch"),
    // export_hashes: type difference for timestamps
    ("export_hashes", "exported_at", "type_mismatch"),
];

/// Check if a column diff is a known/expected difference
fn is_known_column_diff(diff: &ColumnDiff) -> bool {
    // Check issues table for known issues-specific differences
    if diff.table == "issues" {
        return match diff.diff_type.as_str() {
            "missing_in_br" => KNOWN_BD_ONLY_COLUMNS.contains(&diff.column.as_str()),
            "missing_in_bd" => KNOWN_BR_ONLY_COLUMNS.contains(&diff.column.as_str()),
            "type_mismatch" => KNOWN_TYPE_DIFFERENCES.contains(&diff.column.as_str()),
            "notnull_mismatch" => KNOWN_NOTNULL_DIFFERENCES.contains(&diff.column.as_str()),
            _ => false,
        };
    }

    // Check other tables for known differences
    KNOWN_OTHER_TABLE_DIFFS
        .iter()
        .any(|(table, col, diff_type)| {
            diff.table == *table && diff.column == *col && diff.diff_type == *diff_type
        })
}

#[test]
fn conformance_schema_full_comparison() {
    skip_if_no_binaries!();
    common::init_test_logging();
    info!("Starting conformance_schema_full_comparison test");

    let workspace = SchemaWorkspace::new();
    workspace.init_both();

    let comparison = compare_schemas(&workspace);

    // Print detailed summary for debugging
    println!("Schema Comparison Summary:");
    println!("  Common tables: {:?}", comparison.tables_common);
    println!("  Tables in br only: {:?}", comparison.tables_br_only);
    println!("  Tables in bd only: {:?}", comparison.tables_bd_only);
    println!("  Column diffs: {}", comparison.column_diffs.len());
    for diff in &comparison.column_diffs {
        let known = if is_known_column_diff(diff) {
            " [KNOWN]"
        } else {
            ""
        };
        println!(
            "    {}.{}: {} (br: {}, bd: {}){}",
            diff.table, diff.column, diff.diff_type, diff.br_value, diff.bd_value, known
        );
    }
    println!("  Index diffs: {}", comparison.index_diffs.len());

    // Filter out known/expected differences
    let unexpected_table_diffs: Vec<&String> = comparison
        .tables_bd_only
        .iter()
        .filter(|t| !KNOWN_BD_ONLY_TABLES.contains(&t.as_str()))
        .collect();

    let unexpected_column_diffs: Vec<&ColumnDiff> = comparison
        .column_diffs
        .iter()
        .filter(|d| !is_known_column_diff(d))
        .collect();

    // Log known differences for visibility
    let known_table_count = comparison.tables_bd_only.len() - unexpected_table_diffs.len();
    let known_column_count = comparison.column_diffs.len() - unexpected_column_diffs.len();
    if known_table_count > 0 {
        info!(
            "{} known table differences (Gastown features)",
            known_table_count
        );
    }
    if known_column_count > 0 {
        info!(
            "{} known column differences (Gastown features, type/notnull)",
            known_column_count
        );
    }

    // Assert no unexpected differences
    assert!(
        unexpected_table_diffs.is_empty(),
        "Unexpected tables in bd not in br: {:?}",
        unexpected_table_diffs
    );

    assert!(
        unexpected_column_diffs.is_empty(),
        "Unexpected column differences:\n{}",
        unexpected_column_diffs
            .iter()
            .map(|d| format!(
                "  {}.{}: {} (br: {}, bd: {})",
                d.table, d.column, d.diff_type, d.br_value, d.bd_value
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // Index differences are informational only (auto-generated names differ)
    // But log them for visibility
    if !comparison.index_diffs.is_empty() {
        info!(
            "{} index differences (informational, names auto-generated)",
            comparison.index_diffs.len()
        );
    }

    println!("\n[PASS] Schema conformance check passed with known differences documented");
}
