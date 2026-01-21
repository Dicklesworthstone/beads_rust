#![allow(clippy::all, clippy::pedantic, clippy::nursery, dead_code)]
//! Conformance Tests: Multi-Step Mutating Workflows
//!
//! This module tests complex multi-step workflows that involve create, update,
//! close, delete, and dependency operations. It compares br vs bd outcomes
//! with normalization for volatile fields (timestamps, IDs).
//!
//! Key features:
//! - Multi-step workflow sequences
//! - JSONL export comparison with normalization
//! - Field-level diff explanations
//! - Structural parity checking (status, priority, deps, labels, counts)
//!
//! Related beads:
//! - beads_rust-4vzm: Conformance harness: mutating workflows (normalized)

mod common;

use common::cli::extract_json_payload;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;
use tracing::info;

// ============================================================================
// BD AVAILABILITY CHECK
// ============================================================================

/// Get the path to the `bd` (Go beads) binary.
/// Checks `BD_BINARY` environment variable first, falls back to PATH lookup.
fn get_bd_binary() -> String {
    std::env::var("BD_BINARY").unwrap_or_else(|_| "bd".to_string())
}

/// Check if bd (Go beads binary) is available for conformance tests.
/// Returns false if `bd` is aliased/symlinked to `br` (detected via version output).
/// Respects `BD_BINARY` environment variable for custom binary path.
fn bd_available() -> bool {
    let bd_bin = get_bd_binary();
    Command::new(&bd_bin)
        .arg("version")
        .output()
        .is_ok_and(|o| {
            if !o.status.success() {
                return false;
            }
            // Check that this is actually Go bd, not br aliased as bd.
            // Go bd outputs "bd version X" or "beads version X".
            // Rust br outputs "br version X".
            let stdout = String::from_utf8_lossy(&o.stdout);
            let Some(first_token) = stdout.split_whitespace().next() else {
                return false;
            };
            match first_token.to_ascii_lowercase().as_str() {
                "bd" | "beads" => true,
                "br" => false,
                _ => false,
            }
        })
}

/// Skip test if bd is not available (used in CI where bd isn't installed)
macro_rules! skip_if_no_bd {
    () => {
        if !bd_available() {
            eprintln!(
                "Skipping test: 'bd' binary missing or aliased to br. \
                 Set BD_BINARY to a Go bd path for conformance runs."
            );
            return;
        }
    };
}

// ============================================================================
// NORMALIZATION AND COMPARISON HELPERS
// ============================================================================

/// Fields that should be masked during comparison (volatile).
const TIMESTAMP_FIELDS: &[&str] = &[
    "created_at",
    "updated_at",
    "closed_at",
    "defer_until",
    "due_at",
    "deleted_at",
    "compacted_at",
];

/// Fields that are structural and must match exactly.
const STRUCTURAL_FIELDS: &[&str] = &[
    "title",
    "status",
    "priority",
    "type",
    "assignee",
    "labels",
    "depends_on",
    "blocks",
    "external_ref",
    "description",
];

/// Fields that br includes but bd may omit (implementation-specific extras).
/// These are ignored when comparing JSONL outputs to allow for minor serialization differences.
const IGNORABLE_BR_ONLY_FIELDS: &[&str] = &["compaction_level", "original_size", "source_repo"];

/// Fields where br and bd have different implementation-specific defaults.
/// These are audit/actor fields that vary between implementations but don't affect semantics.
const IMPLEMENTATION_SPECIFIC_FIELDS: &[&str] = &["deleted_by", "delete_reason"];

/// Default close_reason values that are semantically equivalent.
/// br uses "done", bd uses "Closed" - both mean the same thing.
const EQUIVALENT_CLOSE_REASONS: &[(&str, &str)] = &[("done", "Closed")];

/// Detailed diff result with field-level explanations.
#[derive(Debug, Default)]
pub struct DiffResult {
    pub matched: bool,
    pub structural_diffs: Vec<FieldDiff>,
    pub timestamp_drifts: Vec<String>,
    pub extra_br_fields: Vec<String>,
    pub extra_bd_fields: Vec<String>,
    pub normalized_log: Vec<String>,
}

#[derive(Debug)]
pub struct FieldDiff {
    pub path: String,
    pub br_value: String,
    pub bd_value: String,
    pub explanation: String,
}

impl DiffResult {
    pub fn explain(&self) -> String {
        let mut parts = Vec::new();

        if !self.structural_diffs.is_empty() {
            parts.push("Structural differences:".to_string());
            for diff in &self.structural_diffs {
                parts.push(format!(
                    "  - {}: br='{}' vs bd='{}' ({})",
                    diff.path, diff.br_value, diff.bd_value, diff.explanation
                ));
            }
        }

        if !self.timestamp_drifts.is_empty() {
            parts.push(format!(
                "Timestamp drifts (within tolerance): {}",
                self.timestamp_drifts.join(", ")
            ));
        }

        if !self.extra_br_fields.is_empty() {
            parts.push(format!(
                "Fields only in br: {}",
                self.extra_br_fields.join(", ")
            ));
        }

        if !self.extra_bd_fields.is_empty() {
            parts.push(format!(
                "Fields only in bd: {}",
                self.extra_bd_fields.join(", ")
            ));
        }

        if parts.is_empty() {
            "No differences found".to_string()
        } else {
            parts.join("\n")
        }
    }
}

/// Normalize a JSON value by masking timestamps and normalizing IDs.
fn normalize_json(value: &mut Value, path: &str, log: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                let field_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{}.{}", path, key)
                };

                // Mask timestamp fields
                if TIMESTAMP_FIELDS.contains(&key.as_str()) {
                    if let Some(s) = val.as_str() {
                        if !s.is_empty() {
                            log.push(format!("Masked timestamp: {}", field_path));
                            *val = Value::String("NORMALIZED_TIMESTAMP".to_string());
                        }
                    }
                }
                // Normalize ID fields
                else if key == "id" || key.ends_with("_id") {
                    if let Some(s) = val.as_str() {
                        if let Some(dash_pos) = s.rfind('-') {
                            let prefix = &s[..dash_pos];
                            log.push(format!("Normalized ID: {} ({})", field_path, s));
                            *val = Value::String(format!("{}-HASH", prefix));
                        }
                    }
                } else {
                    normalize_json(val, &field_path, log);
                }
            }
        }
        Value::Array(arr) => {
            for (i, item) in arr.iter_mut().enumerate() {
                normalize_json(item, &format!("{}[{}]", path, i), log);
            }
            // Sort arrays for deterministic comparison
            arr.sort_by(|a, b| {
                serde_json::to_string(a)
                    .unwrap_or_default()
                    .cmp(&serde_json::to_string(b).unwrap_or_default())
            });
        }
        _ => {}
    }
}

/// Compare two JSON values with field-level diff explanations.
fn compare_json_with_diff(br: &Value, bd: &Value, path: &str, result: &mut DiffResult) {
    match (br, bd) {
        (Value::Object(br_map), Value::Object(bd_map)) => {
            // Check for structural field differences
            let all_keys: HashSet<_> = br_map.keys().chain(bd_map.keys()).collect();

            for key in all_keys {
                let field_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{}.{}", path, key)
                };

                match (br_map.get(key), bd_map.get(key)) {
                    (Some(br_val), Some(bd_val)) => {
                        compare_json_with_diff(br_val, bd_val, &field_path, result);
                    }
                    (Some(_), None) => {
                        // Skip ignorable br-only fields (implementation extras)
                        if !IGNORABLE_BR_ONLY_FIELDS.contains(&key.as_str()) {
                            result.extra_br_fields.push(field_path);
                        }
                    }
                    (None, Some(_)) => {
                        result.extra_bd_fields.push(field_path);
                    }
                    (None, None) => {}
                }
            }
        }
        (Value::Array(br_arr), Value::Array(bd_arr)) => {
            if br_arr.len() != bd_arr.len() {
                result.structural_diffs.push(FieldDiff {
                    path: path.to_string(),
                    br_value: format!("array[{}]", br_arr.len()),
                    bd_value: format!("array[{}]", bd_arr.len()),
                    explanation: "Array length mismatch".to_string(),
                });
            }
            for (i, (br_item, bd_item)) in br_arr.iter().zip(bd_arr.iter()).enumerate() {
                compare_json_with_diff(br_item, bd_item, &format!("{}[{}]", path, i), result);
            }
        }
        _ => {
            if br != bd {
                // Check if this is a close_reason with equivalent values
                let is_equivalent_close_reason = path.ends_with("close_reason")
                    && br
                        .as_str()
                        .zip(bd.as_str())
                        .is_some_and(|(br_str, bd_str)| {
                            EQUIVALENT_CLOSE_REASONS.iter().any(|(br_eq, bd_eq)| {
                                (br_str == *br_eq && bd_str == *bd_eq)
                                    || (br_str == *bd_eq && bd_str == *br_eq)
                            })
                        });

                // Skip implementation-specific fields that differ between br and bd
                let is_implementation_specific = IMPLEMENTATION_SPECIFIC_FIELDS
                    .iter()
                    .any(|f| path.ends_with(f));

                if is_equivalent_close_reason || is_implementation_specific {
                    // Skip - these are either semantically equivalent or implementation-specific
                } else {
                    let is_structural = STRUCTURAL_FIELDS.iter().any(|f| path.ends_with(f));
                    if is_structural || !path.contains("NORMALIZED") {
                        result.structural_diffs.push(FieldDiff {
                            path: path.to_string(),
                            br_value: format!("{:?}", br),
                            bd_value: format!("{:?}", bd),
                            explanation: if is_structural {
                                "Structural field mismatch".to_string()
                            } else {
                                "Value mismatch".to_string()
                            },
                        });
                    }
                }
            }
        }
    }
}

/// Compare JSONL files with normalization and field-level diffs.
fn compare_jsonl_files(br_path: &Path, bd_path: &Path) -> DiffResult {
    let mut result = DiffResult::default();

    let br_content = fs::read_to_string(br_path).unwrap_or_default();
    let bd_content = fs::read_to_string(bd_path).unwrap_or_default();

    // Parse JSONL lines
    let br_entries: Vec<Value> = br_content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    let bd_entries: Vec<Value> = bd_content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    if br_entries.len() != bd_entries.len() {
        result.structural_diffs.push(FieldDiff {
            path: "jsonl_line_count".to_string(),
            br_value: format!("{}", br_entries.len()),
            bd_value: format!("{}", bd_entries.len()),
            explanation: "JSONL line count mismatch".to_string(),
        });
    }

    // Normalize both sets
    let mut br_normalized: Vec<Value> = br_entries.clone();
    let mut bd_normalized: Vec<Value> = bd_entries.clone();

    for entry in &mut br_normalized {
        normalize_json(entry, "", &mut result.normalized_log);
    }
    for entry in &mut bd_normalized {
        normalize_json(entry, "", &mut result.normalized_log);
    }

    // Sort by title for deterministic comparison
    br_normalized.sort_by(|a, b| {
        let a_title = a.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let b_title = b.get("title").and_then(|v| v.as_str()).unwrap_or("");
        a_title.cmp(b_title)
    });
    bd_normalized.sort_by(|a, b| {
        let a_title = a.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let b_title = b.get("title").and_then(|v| v.as_str()).unwrap_or("");
        a_title.cmp(b_title)
    });

    // Compare entry by entry
    for (i, (br_entry, bd_entry)) in br_normalized.iter().zip(bd_normalized.iter()).enumerate() {
        compare_json_with_diff(br_entry, bd_entry, &format!("entry[{}]", i), &mut result);
    }

    result.matched = result.structural_diffs.is_empty()
        && result.extra_br_fields.is_empty()
        && result.extra_bd_fields.is_empty();

    result
}

// ============================================================================
// WORKFLOW WORKSPACE
// ============================================================================

/// Workspace for multi-step workflow conformance tests.
pub struct WorkflowWorkspace {
    pub temp_dir: TempDir,
    pub br_root: std::path::PathBuf,
    pub bd_root: std::path::PathBuf,
    pub log_dir: std::path::PathBuf,
    pub workflow_log: Vec<WorkflowStep>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowStep {
    pub step_num: usize,
    pub command: String,
    pub br_success: bool,
    pub bd_success: bool,
    pub br_stdout_len: usize,
    pub bd_stdout_len: usize,
}

impl WorkflowWorkspace {
    pub fn new(_name: &str) -> Self {
        let temp_dir = TempDir::new().expect("create temp dir");
        let root = temp_dir.path().to_path_buf();
        let br_root = root.join("br_workspace");
        let bd_root = root.join("bd_workspace");
        let log_dir = root.join("logs");

        fs::create_dir_all(&br_root).expect("create br workspace");
        fs::create_dir_all(&bd_root).expect("create bd workspace");
        fs::create_dir_all(&log_dir).expect("create log dir");

        // Initialize git repos (required for beads)
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&br_root)
            .output()
            .ok();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&bd_root)
            .output()
            .ok();

        Self {
            temp_dir,
            br_root,
            bd_root,
            log_dir,
            workflow_log: Vec::new(),
        }
    }

    /// Initialize both br and bd workspaces with consistent prefix.
    pub fn init_both(&mut self) {
        // Use explicit --prefix bd to ensure both tools use the same prefix.
        // bd defaults to directory name, br defaults to "bd", so we need parity.
        self.run_step(0, &["init", "--prefix", "bd"]);
    }

    /// Run a command on both br and bd, logging the results.
    pub fn run_step(&mut self, step_num: usize, args: &[&str]) -> (CmdOutput, CmdOutput) {
        let br_out = self.run_br(args);
        let bd_out = self.run_bd(args);

        self.workflow_log.push(WorkflowStep {
            step_num,
            command: args.join(" "),
            br_success: br_out.status.success(),
            bd_success: bd_out.status.success(),
            br_stdout_len: br_out.stdout.len(),
            bd_stdout_len: bd_out.stdout.len(),
        });

        (br_out, bd_out)
    }

    /// Run br command.
    pub fn run_br(&self, args: &[&str]) -> CmdOutput {
        let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin!("br"));
        cmd.current_dir(&self.br_root);
        cmd.args(args);
        cmd.env("NO_COLOR", "1");
        cmd.env("HOME", &self.br_root);

        let start = std::time::Instant::now();
        let output = cmd.output().expect("run br");
        let duration = start.elapsed();

        CmdOutput {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            status: output.status,
            duration,
        }
    }

    /// Run bd command.
    /// Respects `BD_BINARY` environment variable for custom binary path.
    pub fn run_bd(&self, args: &[&str]) -> CmdOutput {
        let mut cmd = std::process::Command::new(get_bd_binary());
        cmd.current_dir(&self.bd_root);
        cmd.args(args);
        cmd.env("NO_COLOR", "1");
        cmd.env("HOME", &self.bd_root);

        let start = std::time::Instant::now();
        let output = cmd.output().expect("run bd");
        let duration = start.elapsed();

        CmdOutput {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            status: output.status,
            duration,
        }
    }

    /// Get the JSONL file paths.
    pub fn jsonl_paths(&self) -> (std::path::PathBuf, std::path::PathBuf) {
        (
            self.br_root.join(".beads").join("issues.jsonl"),
            self.bd_root.join(".beads").join("issues.jsonl"),
        )
    }

    /// Compare JSONL exports with field-level diff.
    pub fn compare_jsonl(&self) -> DiffResult {
        let (br_jsonl, bd_jsonl) = self.jsonl_paths();
        compare_jsonl_files(&br_jsonl, &bd_jsonl)
    }

    /// Flush both workspaces to JSONL.
    pub fn flush_both(&mut self, step_num: usize) -> (CmdOutput, CmdOutput) {
        self.run_step(step_num, &["sync", "--flush-only"])
    }

    /// Write workflow log to file.
    pub fn write_log(&self) {
        let log_path = self.log_dir.join("workflow.json");
        let json = serde_json::to_string_pretty(&self.workflow_log).unwrap_or_default();
        fs::write(&log_path, json).ok();
    }

    /// Extract issue ID from create output (handles both br and bd formats).
    pub fn extract_id(output: &str) -> Option<String> {
        let json_str = extract_json_payload(output);
        if let Ok(val) = serde_json::from_str::<Value>(&json_str) {
            // Try direct id field
            if let Some(id) = val.get("id").and_then(|v| v.as_str()) {
                return Some(id.to_string());
            }
            // Try array format
            if let Some(arr) = val.as_array() {
                if let Some(first) = arr.first() {
                    if let Some(id) = first.get("id").and_then(|v| v.as_str()) {
                        return Some(id.to_string());
                    }
                }
            }
        }
        None
    }
}

#[derive(Debug)]
pub struct CmdOutput {
    pub stdout: String,
    pub stderr: String,
    pub status: std::process::ExitStatus,
    pub duration: std::time::Duration,
}

// ============================================================================
// MULTI-STEP WORKFLOW TESTS
// ============================================================================

/// Test: Create multiple issues, update various fields, verify final state.
#[test]
fn conformance_workflow_create_update_lifecycle() {
    skip_if_no_bd!();
    common::init_test_logging();
    info!("Starting conformance_workflow_create_update_lifecycle");

    let mut ws = WorkflowWorkspace::new("create_update_lifecycle");
    ws.init_both();

    // Step 1: Create issues with different types and priorities
    let (br_c1, bd_c1) = ws.run_step(
        1,
        &[
            "create",
            "Feature A",
            "--type",
            "feature",
            "--priority",
            "1",
            "--json",
        ],
    );
    assert!(br_c1.status.success(), "br create failed: {}", br_c1.stderr);
    assert!(bd_c1.status.success(), "bd create failed: {}", bd_c1.stderr);

    let (br_c2, bd_c2) = ws.run_step(
        2,
        &[
            "create",
            "Bug B",
            "--type",
            "bug",
            "--priority",
            "0",
            "--json",
        ],
    );
    assert!(br_c2.status.success());
    assert!(bd_c2.status.success());

    let (br_c3, bd_c3) = ws.run_step(
        3,
        &[
            "create",
            "Task C",
            "--type",
            "task",
            "--priority",
            "2",
            "--json",
        ],
    );
    assert!(br_c3.status.success());
    assert!(bd_c3.status.success());

    // Extract IDs
    let br_id1 = WorkflowWorkspace::extract_id(&br_c1.stdout).expect("br id1");
    let bd_id1 = WorkflowWorkspace::extract_id(&bd_c1.stdout).expect("bd id1");
    let br_id2 = WorkflowWorkspace::extract_id(&br_c2.stdout).expect("br id2");
    let bd_id2 = WorkflowWorkspace::extract_id(&bd_c2.stdout).expect("bd id2");

    // Step 4: Update status on first issue
    let (br_u1, bd_u1) = (
        ws.run_br(&["update", &br_id1, "--status", "in_progress", "--json"]),
        ws.run_bd(&["update", &bd_id1, "--status", "in_progress", "--json"]),
    );
    assert!(br_u1.status.success(), "br update failed: {}", br_u1.stderr);
    assert!(bd_u1.status.success(), "bd update failed: {}", bd_u1.stderr);

    // Step 5: Update priority on second issue
    let (br_u2, bd_u2) = (
        ws.run_br(&["update", &br_id2, "--priority", "1", "--json"]),
        ws.run_bd(&["update", &bd_id2, "--priority", "1", "--json"]),
    );
    assert!(br_u2.status.success());
    assert!(bd_u2.status.success());

    // Step 6: Verify list output structure matches
    let (br_list, bd_list) = ws.run_step(6, &["list", "--json"]);
    assert!(br_list.status.success());
    assert!(bd_list.status.success());

    let br_json = extract_json_payload(&br_list.stdout);
    let bd_json = extract_json_payload(&bd_list.stdout);

    let mut br_val: Value = serde_json::from_str(&br_json).expect("parse br");
    let mut bd_val: Value = serde_json::from_str(&bd_json).expect("parse bd");

    // Normalize and compare
    let mut log = Vec::new();
    normalize_json(&mut br_val, "", &mut log);
    normalize_json(&mut bd_val, "", &mut log);

    // Count issues
    let br_count = br_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_count = bd_val.as_array().map(|a| a.len()).unwrap_or(0);
    assert_eq!(
        br_count, bd_count,
        "Issue counts differ: br={}, bd={}",
        br_count, bd_count
    );
    assert_eq!(br_count, 3, "Expected 3 issues");

    // Flush and compare JSONL
    ws.flush_both(7);
    let diff = ws.compare_jsonl();
    ws.write_log();

    assert!(diff.matched, "JSONL comparison failed:\n{}", diff.explain());

    info!("conformance_workflow_create_update_lifecycle passed");
}

/// Test: Create issues with dependencies, verify blocked/ready states.
#[test]
fn conformance_workflow_dependency_chain() {
    skip_if_no_bd!();
    common::init_test_logging();
    info!("Starting conformance_workflow_dependency_chain");

    let mut ws = WorkflowWorkspace::new("dependency_chain");
    ws.init_both();

    // Create a chain: A blocks B blocks C
    let (br_ca, bd_ca) = ws.run_step(1, &["create", "Foundation A", "--json"]);
    assert!(br_ca.status.success());
    assert!(bd_ca.status.success());

    let (br_cb, bd_cb) = ws.run_step(2, &["create", "Build on A", "--json"]);
    assert!(br_cb.status.success());
    assert!(bd_cb.status.success());

    let (br_cc, bd_cc) = ws.run_step(3, &["create", "Final C", "--json"]);
    assert!(br_cc.status.success());
    assert!(bd_cc.status.success());

    let br_id_a = WorkflowWorkspace::extract_id(&br_ca.stdout).expect("br A");
    let bd_id_a = WorkflowWorkspace::extract_id(&bd_ca.stdout).expect("bd A");
    let br_id_b = WorkflowWorkspace::extract_id(&br_cb.stdout).expect("br B");
    let bd_id_b = WorkflowWorkspace::extract_id(&bd_cb.stdout).expect("bd B");
    let br_id_c = WorkflowWorkspace::extract_id(&br_cc.stdout).expect("br C");
    let bd_id_c = WorkflowWorkspace::extract_id(&bd_cc.stdout).expect("bd C");

    // Add dependencies: B depends on A, C depends on B
    let br_dep1 = ws.run_br(&["dep", "add", &br_id_b, &br_id_a]);
    let bd_dep1 = ws.run_bd(&["dep", "add", &bd_id_b, &bd_id_a]);
    assert!(
        br_dep1.status.success(),
        "br dep add failed: {}",
        br_dep1.stderr
    );
    assert!(
        bd_dep1.status.success(),
        "bd dep add failed: {}",
        bd_dep1.stderr
    );

    let br_dep2 = ws.run_br(&["dep", "add", &br_id_c, &br_id_b]);
    let bd_dep2 = ws.run_bd(&["dep", "add", &bd_id_c, &bd_id_b]);
    assert!(br_dep2.status.success());
    assert!(bd_dep2.status.success());

    // Verify blocked command shows B and C as blocked
    let (br_blocked, bd_blocked) = ws.run_step(6, &["blocked", "--json"]);
    assert!(br_blocked.status.success());
    assert!(bd_blocked.status.success());

    let br_blocked_json = extract_json_payload(&br_blocked.stdout);
    let bd_blocked_json = extract_json_payload(&bd_blocked.stdout);

    let br_blocked_val: Value =
        serde_json::from_str(&br_blocked_json).unwrap_or(Value::Array(vec![]));
    let bd_blocked_val: Value =
        serde_json::from_str(&bd_blocked_json).unwrap_or(Value::Array(vec![]));

    let br_blocked_count = br_blocked_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_blocked_count = bd_blocked_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_blocked_count, bd_blocked_count,
        "Blocked counts differ: br={}, bd={}",
        br_blocked_count, bd_blocked_count
    );
    assert_eq!(br_blocked_count, 2, "Expected 2 blocked issues (B and C)");

    // Verify ready command shows only A
    let (br_ready, bd_ready) = ws.run_step(7, &["ready", "--json"]);
    assert!(br_ready.status.success());
    assert!(bd_ready.status.success());

    let br_ready_json = extract_json_payload(&br_ready.stdout);
    let bd_ready_json = extract_json_payload(&bd_ready.stdout);

    let br_ready_val: Value = serde_json::from_str(&br_ready_json).unwrap_or(Value::Array(vec![]));
    let bd_ready_val: Value = serde_json::from_str(&bd_ready_json).unwrap_or(Value::Array(vec![]));

    let br_ready_count = br_ready_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_ready_count = bd_ready_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_ready_count, bd_ready_count,
        "Ready counts differ: br={}, bd={}",
        br_ready_count, bd_ready_count
    );
    assert_eq!(br_ready_count, 1, "Expected 1 ready issue (A)");

    // Flush and compare JSONL
    ws.flush_both(8);
    let diff = ws.compare_jsonl();
    ws.write_log();

    assert!(diff.matched, "JSONL comparison failed:\n{}", diff.explain());

    info!("conformance_workflow_dependency_chain passed");
}

/// Test: Close issues and verify state changes + stats.
#[test]
fn conformance_workflow_close_with_stats() {
    skip_if_no_bd!();
    common::init_test_logging();
    info!("Starting conformance_workflow_close_with_stats");

    let mut ws = WorkflowWorkspace::new("close_with_stats");
    ws.init_both();

    // Create several issues
    let (br_c1, bd_c1) = ws.run_step(1, &["create", "Issue 1", "--json"]);
    let (br_c2, bd_c2) = ws.run_step(2, &["create", "Issue 2", "--json"]);
    let (br_c3, bd_c3) = ws.run_step(3, &["create", "Issue 3", "--json"]);

    assert!(br_c1.status.success() && bd_c1.status.success());
    assert!(br_c2.status.success() && bd_c2.status.success());
    assert!(br_c3.status.success() && bd_c3.status.success());

    let br_id1 = WorkflowWorkspace::extract_id(&br_c1.stdout).expect("id1");
    let bd_id1 = WorkflowWorkspace::extract_id(&bd_c1.stdout).expect("id1");
    let br_id2 = WorkflowWorkspace::extract_id(&br_c2.stdout).expect("id2");
    let bd_id2 = WorkflowWorkspace::extract_id(&bd_c2.stdout).expect("id2");

    // Close two issues
    let br_close1 = ws.run_br(&["close", &br_id1]);
    let bd_close1 = ws.run_bd(&["close", &bd_id1]);
    assert!(br_close1.status.success());
    assert!(bd_close1.status.success());

    let br_close2 = ws.run_br(&["close", &br_id2]);
    let bd_close2 = ws.run_bd(&["close", &bd_id2]);
    assert!(br_close2.status.success());
    assert!(bd_close2.status.success());

    // Check stats
    let (br_stats, bd_stats) = ws.run_step(6, &["stats", "--json"]);
    assert!(br_stats.status.success());
    assert!(bd_stats.status.success());

    let br_stats_json = extract_json_payload(&br_stats.stdout);
    let bd_stats_json = extract_json_payload(&bd_stats.stdout);

    let br_stats_val: Value = serde_json::from_str(&br_stats_json).expect("parse");
    let bd_stats_val: Value = serde_json::from_str(&bd_stats_json).expect("parse");

    // Compare key stats fields
    let br_open = br_stats_val
        .get("open")
        .or_else(|| br_stats_val.get("summary").and_then(|s| s.get("open")));
    let bd_open = bd_stats_val
        .get("open")
        .or_else(|| bd_stats_val.get("summary").and_then(|s| s.get("open")));

    let br_closed = br_stats_val
        .get("closed")
        .or_else(|| br_stats_val.get("summary").and_then(|s| s.get("closed")));
    let bd_closed = bd_stats_val
        .get("closed")
        .or_else(|| bd_stats_val.get("summary").and_then(|s| s.get("closed")));

    assert_eq!(
        br_open.and_then(|v| v.as_i64()),
        bd_open.and_then(|v| v.as_i64()),
        "Open counts differ: br={:?}, bd={:?}",
        br_open,
        bd_open
    );

    assert_eq!(
        br_closed.and_then(|v| v.as_i64()),
        bd_closed.and_then(|v| v.as_i64()),
        "Closed counts differ: br={:?}, bd={:?}",
        br_closed,
        bd_closed
    );

    // Flush and compare JSONL
    ws.flush_both(7);
    let diff = ws.compare_jsonl();
    ws.write_log();

    assert!(diff.matched, "JSONL comparison failed:\n{}", diff.explain());

    info!("conformance_workflow_close_with_stats passed");
}

/// Test: Delete issues and verify they don't appear in list.
#[test]
fn conformance_workflow_delete_lifecycle() {
    skip_if_no_bd!();
    common::init_test_logging();
    info!("Starting conformance_workflow_delete_lifecycle");

    let mut ws = WorkflowWorkspace::new("delete_lifecycle");
    ws.init_both();

    // Create issues
    let (br_c1, bd_c1) = ws.run_step(1, &["create", "To be deleted", "--json"]);
    let (br_c2, bd_c2) = ws.run_step(2, &["create", "Keep this one", "--json"]);

    assert!(br_c1.status.success() && bd_c1.status.success());
    assert!(br_c2.status.success() && bd_c2.status.success());

    let br_id1 = WorkflowWorkspace::extract_id(&br_c1.stdout).expect("id1");
    let bd_id1 = WorkflowWorkspace::extract_id(&bd_c1.stdout).expect("id1");

    // Delete first issue (bd requires --force)
    let br_del = ws.run_br(&["delete", &br_id1, "--reason", "test"]);
    let bd_del = ws.run_bd(&["delete", &bd_id1, "--reason", "test", "--force"]);
    assert!(
        br_del.status.success(),
        "br delete failed: {}",
        br_del.stderr
    );
    assert!(
        bd_del.status.success(),
        "bd delete failed: {}",
        bd_del.stderr
    );

    // Verify list shows only one issue
    let (br_list, bd_list) = ws.run_step(4, &["list", "--json"]);
    assert!(br_list.status.success());
    assert!(bd_list.status.success());

    let br_list_json = extract_json_payload(&br_list.stdout);
    let bd_list_json = extract_json_payload(&bd_list.stdout);

    let br_list_val: Value = serde_json::from_str(&br_list_json).unwrap_or(Value::Array(vec![]));
    let bd_list_val: Value = serde_json::from_str(&bd_list_json).unwrap_or(Value::Array(vec![]));

    let br_count = br_list_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_count = bd_list_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_count, bd_count,
        "List counts differ after delete: br={}, bd={}",
        br_count, bd_count
    );
    assert_eq!(br_count, 1, "Expected 1 issue after deletion");

    // Flush and compare JSONL
    ws.flush_both(5);
    let diff = ws.compare_jsonl();
    ws.write_log();

    assert!(diff.matched, "JSONL comparison failed:\n{}", diff.explain());

    info!("conformance_workflow_delete_lifecycle passed");
}

/// Test: Complete workflow with create, update, deps, close, and delete.
#[test]
fn conformance_workflow_full_lifecycle() {
    skip_if_no_bd!();
    common::init_test_logging();
    info!("Starting conformance_workflow_full_lifecycle");

    let mut ws = WorkflowWorkspace::new("full_lifecycle");
    ws.init_both();

    // Phase 1: Create issues
    let (br_epic, bd_epic) = ws.run_step(
        1,
        &[
            "create",
            "Epic: New Feature",
            "--type",
            "epic",
            "--priority",
            "1",
            "--json",
        ],
    );
    let (br_task1, bd_task1) = ws.run_step(
        2,
        &[
            "create",
            "Task 1: Design",
            "--type",
            "task",
            "--priority",
            "2",
            "--json",
        ],
    );
    let (br_task2, bd_task2) = ws.run_step(
        3,
        &[
            "create",
            "Task 2: Implement",
            "--type",
            "task",
            "--priority",
            "2",
            "--json",
        ],
    );
    let (br_bug, bd_bug) = ws.run_step(
        4,
        &[
            "create",
            "Bug: Edge case",
            "--type",
            "bug",
            "--priority",
            "0",
            "--json",
        ],
    );

    assert!(br_epic.status.success() && bd_epic.status.success());
    assert!(br_task1.status.success() && bd_task1.status.success());
    assert!(br_task2.status.success() && bd_task2.status.success());
    assert!(br_bug.status.success() && bd_bug.status.success());

    let br_epic_id = WorkflowWorkspace::extract_id(&br_epic.stdout).expect("epic id");
    let bd_epic_id = WorkflowWorkspace::extract_id(&bd_epic.stdout).expect("epic id");
    let br_task1_id = WorkflowWorkspace::extract_id(&br_task1.stdout).expect("task1 id");
    let bd_task1_id = WorkflowWorkspace::extract_id(&bd_task1.stdout).expect("task1 id");
    let br_task2_id = WorkflowWorkspace::extract_id(&br_task2.stdout).expect("task2 id");
    let bd_task2_id = WorkflowWorkspace::extract_id(&bd_task2.stdout).expect("task2 id");
    let br_bug_id = WorkflowWorkspace::extract_id(&br_bug.stdout).expect("bug id");
    let bd_bug_id = WorkflowWorkspace::extract_id(&bd_bug.stdout).expect("bug id");

    // Phase 2: Add dependencies
    // Task 1 and Task 2 depend on Epic
    ws.run_br(&["dep", "add", &br_task1_id, &br_epic_id]);
    ws.run_bd(&["dep", "add", &bd_task1_id, &bd_epic_id]);
    ws.run_br(&["dep", "add", &br_task2_id, &br_epic_id]);
    ws.run_bd(&["dep", "add", &bd_task2_id, &bd_epic_id]);

    // Phase 3: Update statuses
    ws.run_br(&["update", &br_epic_id, "--status", "in_progress"]);
    ws.run_bd(&["update", &bd_epic_id, "--status", "in_progress"]);

    // Phase 4: Close the epic (this should unblock tasks)
    ws.run_br(&["close", &br_epic_id]);
    ws.run_bd(&["close", &bd_epic_id]);

    // Phase 5: Close task 1
    ws.run_br(&["close", &br_task1_id]);
    ws.run_bd(&["close", &bd_task1_id]);

    // Phase 6: Delete the bug (changed requirements)
    ws.run_br(&["delete", &br_bug_id, "--reason", "no longer relevant"]);
    ws.run_bd(&[
        "delete",
        &bd_bug_id,
        "--reason",
        "no longer relevant",
        "--force",
    ]);

    // Verify final state
    let (br_list, bd_list) = ws.run_step(12, &["list", "--status=all", "--json"]);

    // Some implementations may not support --status=all, try alternative
    let (br_list_final, bd_list_final) = if !br_list.status.success() {
        ws.run_step(12, &["list", "--json"])
    } else {
        (br_list, bd_list)
    };

    let br_json = extract_json_payload(&br_list_final.stdout);
    let bd_json = extract_json_payload(&bd_list_final.stdout);

    let br_val: Value = serde_json::from_str(&br_json).unwrap_or(Value::Array(vec![]));
    let bd_val: Value = serde_json::from_str(&bd_json).unwrap_or(Value::Array(vec![]));

    let br_count = br_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_count = bd_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_count, bd_count,
        "Final list counts differ: br={}, bd={}",
        br_count, bd_count
    );

    // Flush and compare JSONL
    ws.flush_both(13);
    let diff = ws.compare_jsonl();
    ws.write_log();

    // Log any differences for debugging
    if !diff.matched {
        eprintln!("JSONL differences found:\n{}", diff.explain());
    }

    assert!(diff.matched, "JSONL comparison failed:\n{}", diff.explain());

    info!("conformance_workflow_full_lifecycle passed");
}

/// Test: Dependency removal workflow.
#[test]
fn conformance_workflow_dep_removal() {
    skip_if_no_bd!();
    common::init_test_logging();
    info!("Starting conformance_workflow_dep_removal");

    let mut ws = WorkflowWorkspace::new("dep_removal");
    ws.init_both();

    // Create two issues
    let (br_c1, bd_c1) = ws.run_step(1, &["create", "Blocker issue", "--json"]);
    let (br_c2, bd_c2) = ws.run_step(2, &["create", "Blocked issue", "--json"]);

    assert!(br_c1.status.success() && bd_c1.status.success());
    assert!(br_c2.status.success() && bd_c2.status.success());

    let br_id1 = WorkflowWorkspace::extract_id(&br_c1.stdout).expect("id1");
    let bd_id1 = WorkflowWorkspace::extract_id(&bd_c1.stdout).expect("id1");
    let br_id2 = WorkflowWorkspace::extract_id(&br_c2.stdout).expect("id2");
    let bd_id2 = WorkflowWorkspace::extract_id(&bd_c2.stdout).expect("id2");

    // Add dependency
    let br_add = ws.run_br(&["dep", "add", &br_id2, &br_id1]);
    let bd_add = ws.run_bd(&["dep", "add", &bd_id2, &bd_id1]);
    assert!(br_add.status.success());
    assert!(bd_add.status.success());

    // Verify blocked
    let (br_blocked1, bd_blocked1) = ws.run_step(4, &["blocked", "--json"]);
    let br_blocked1_val: Value = serde_json::from_str(&extract_json_payload(&br_blocked1.stdout))
        .unwrap_or(Value::Array(vec![]));
    let bd_blocked1_val: Value = serde_json::from_str(&extract_json_payload(&bd_blocked1.stdout))
        .unwrap_or(Value::Array(vec![]));

    let br_blocked1_count = br_blocked1_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_blocked1_count = bd_blocked1_val.as_array().map(|a| a.len()).unwrap_or(0);
    assert_eq!(br_blocked1_count, bd_blocked1_count);
    assert_eq!(
        br_blocked1_count, 1,
        "Expected 1 blocked issue before removal"
    );

    // Remove dependency
    let br_rm = ws.run_br(&["dep", "rm", &br_id2, &br_id1]);
    let bd_rm = ws.run_bd(&["dep", "rm", &bd_id2, &bd_id1]);
    assert!(br_rm.status.success(), "br dep rm failed: {}", br_rm.stderr);
    assert!(bd_rm.status.success(), "bd dep rm failed: {}", bd_rm.stderr);

    // Verify no longer blocked
    let (br_blocked2, bd_blocked2) = ws.run_step(6, &["blocked", "--json"]);
    let br_blocked2_val: Value = serde_json::from_str(&extract_json_payload(&br_blocked2.stdout))
        .unwrap_or(Value::Array(vec![]));
    let bd_blocked2_val: Value = serde_json::from_str(&extract_json_payload(&bd_blocked2.stdout))
        .unwrap_or(Value::Array(vec![]));

    let br_blocked2_count = br_blocked2_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_blocked2_count = bd_blocked2_val.as_array().map(|a| a.len()).unwrap_or(0);
    assert_eq!(br_blocked2_count, bd_blocked2_count);
    assert_eq!(
        br_blocked2_count, 0,
        "Expected 0 blocked issues after removal"
    );

    // Flush and compare JSONL
    ws.flush_both(7);
    let diff = ws.compare_jsonl();
    ws.write_log();

    assert!(diff.matched, "JSONL comparison failed:\n{}", diff.explain());

    info!("conformance_workflow_dep_removal passed");
}

/// Test: Multiple updates to same issue.
#[test]
fn conformance_workflow_sequential_updates() {
    skip_if_no_bd!();
    common::init_test_logging();
    info!("Starting conformance_workflow_sequential_updates");

    let mut ws = WorkflowWorkspace::new("sequential_updates");
    ws.init_both();

    // Create issue
    let (br_c, bd_c) = ws.run_step(
        1,
        &[
            "create",
            "Evolving issue",
            "--type",
            "task",
            "--priority",
            "3",
            "--json",
        ],
    );
    assert!(br_c.status.success() && bd_c.status.success());

    let br_id = WorkflowWorkspace::extract_id(&br_c.stdout).expect("id");
    let bd_id = WorkflowWorkspace::extract_id(&bd_c.stdout).expect("id");

    // Sequence of updates
    // Update 1: Change priority
    ws.run_br(&["update", &br_id, "--priority", "2"]);
    ws.run_bd(&["update", &bd_id, "--priority", "2"]);

    // Update 2: Change status
    ws.run_br(&["update", &br_id, "--status", "in_progress"]);
    ws.run_bd(&["update", &bd_id, "--status", "in_progress"]);

    // Update 3: Change priority again
    ws.run_br(&["update", &br_id, "--priority", "1"]);
    ws.run_bd(&["update", &bd_id, "--priority", "1"]);

    // Update 4: Change type
    ws.run_br(&["update", &br_id, "--type", "bug"]);
    ws.run_bd(&["update", &bd_id, "--type", "bug"]);

    // Verify final state
    let br_show = ws.run_br(&["show", &br_id, "--json"]);
    let bd_show = ws.run_bd(&["show", &bd_id, "--json"]);

    assert!(br_show.status.success());
    assert!(bd_show.status.success());

    let br_show_json = extract_json_payload(&br_show.stdout);
    let bd_show_json = extract_json_payload(&bd_show.stdout);

    let br_val: Value = serde_json::from_str(&br_show_json).expect("parse");
    let bd_val: Value = serde_json::from_str(&bd_show_json).expect("parse");

    // Handle array response
    let br_issue = if br_val.is_array() {
        br_val[0].clone()
    } else {
        br_val.clone()
    };
    let bd_issue = if bd_val.is_array() {
        bd_val[0].clone()
    } else {
        bd_val.clone()
    };

    // Check structural fields match
    assert_eq!(
        br_issue.get("priority").and_then(|v| v.as_i64()),
        bd_issue.get("priority").and_then(|v| v.as_i64()),
        "Priority mismatch: br={:?}, bd={:?}",
        br_issue.get("priority"),
        bd_issue.get("priority")
    );

    assert_eq!(
        br_issue.get("status").and_then(|v| v.as_str()),
        bd_issue.get("status").and_then(|v| v.as_str()),
        "Status mismatch"
    );

    assert_eq!(
        br_issue.get("type").and_then(|v| v.as_str()),
        bd_issue.get("type").and_then(|v| v.as_str()),
        "Type mismatch"
    );

    // Flush and compare JSONL
    ws.flush_both(7);
    let diff = ws.compare_jsonl();
    ws.write_log();

    assert!(diff.matched, "JSONL comparison failed:\n{}", diff.explain());

    info!("conformance_workflow_sequential_updates passed");
}

/// Test: Workflow with assignee changes.
#[test]
fn conformance_workflow_assignee_changes() {
    skip_if_no_bd!();
    common::init_test_logging();
    info!("Starting conformance_workflow_assignee_changes");

    let mut ws = WorkflowWorkspace::new("assignee_changes");
    ws.init_both();

    // Create issue with assignee
    let (br_c, bd_c) = ws.run_step(
        1,
        &["create", "Assigned task", "--assignee", "alice", "--json"],
    );
    assert!(br_c.status.success() && bd_c.status.success());

    let br_id = WorkflowWorkspace::extract_id(&br_c.stdout).expect("id");
    let bd_id = WorkflowWorkspace::extract_id(&bd_c.stdout).expect("id");

    // Reassign to different person
    let br_u1 = ws.run_br(&["update", &br_id, "--assignee", "bob"]);
    let bd_u1 = ws.run_bd(&["update", &bd_id, "--assignee", "bob"]);
    assert!(br_u1.status.success());
    assert!(bd_u1.status.success());

    // Verify assignee
    let br_show = ws.run_br(&["show", &br_id, "--json"]);
    let bd_show = ws.run_bd(&["show", &bd_id, "--json"]);

    let br_val: Value =
        serde_json::from_str(&extract_json_payload(&br_show.stdout)).expect("parse");
    let bd_val: Value =
        serde_json::from_str(&extract_json_payload(&bd_show.stdout)).expect("parse");

    let br_issue = if br_val.is_array() {
        &br_val[0]
    } else {
        &br_val
    };
    let bd_issue = if bd_val.is_array() {
        &bd_val[0]
    } else {
        &bd_val
    };

    assert_eq!(
        br_issue.get("assignee").and_then(|v| v.as_str()),
        bd_issue.get("assignee").and_then(|v| v.as_str()),
        "Assignee mismatch: br={:?}, bd={:?}",
        br_issue.get("assignee"),
        bd_issue.get("assignee")
    );

    // Flush and compare JSONL
    ws.flush_both(4);
    let diff = ws.compare_jsonl();
    ws.write_log();

    assert!(diff.matched, "JSONL comparison failed:\n{}", diff.explain());

    info!("conformance_workflow_assignee_changes passed");
}
