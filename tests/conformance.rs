//! Conformance Tests: Validate br (Rust) produces identical output to bd (Go)
//!
//! This harness runs equivalent commands on both br and bd in isolated temp directories,
//! then compares outputs using various comparison modes.

mod common;

use assert_cmd::Command;
use common::cli::extract_json_payload;
use serde_json::Value;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};
use tempfile::TempDir;
use tracing::info;

/// Output from running a command
#[derive(Debug)]
pub struct CmdOutput {
    pub stdout: String,
    pub stderr: String,
    pub status: std::process::ExitStatus,
    pub duration: Duration,
}

/// Workspace for conformance tests with paired br/bd directories
pub struct ConformanceWorkspace {
    pub temp_dir: TempDir,
    pub br_root: PathBuf,
    pub bd_root: PathBuf,
    pub log_dir: PathBuf,
}

impl ConformanceWorkspace {
    pub fn new() -> Self {
        let temp_dir = TempDir::new().expect("create temp dir");
        let root = temp_dir.path().to_path_buf();
        let br_root = root.join("br_workspace");
        let bd_root = root.join("bd_workspace");
        let log_dir = root.join("logs");

        fs::create_dir_all(&br_root).expect("create br workspace");
        fs::create_dir_all(&bd_root).expect("create bd workspace");
        fs::create_dir_all(&log_dir).expect("create log dir");

        Self {
            temp_dir,
            br_root,
            bd_root,
            log_dir,
        }
    }

    /// Initialize both br and bd workspaces
    pub fn init_both(&self) -> (CmdOutput, CmdOutput) {
        let br_out = self.run_br(["init"], "init");
        let bd_out = self.run_bd(["init"], "init");
        (br_out, bd_out)
    }

    /// Run br command in the br workspace
    pub fn run_br<I, S>(&self, args: I, label: &str) -> CmdOutput
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        run_br_cmd(&self.br_root, &self.log_dir, args, &format!("br_{label}"))
    }

    /// Run bd command in the bd workspace
    pub fn run_bd<I, S>(&self, args: I, label: &str) -> CmdOutput
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        run_bd_cmd(&self.bd_root, &self.log_dir, args, &format!("bd_{label}"))
    }
}

fn run_br_cmd<I, S>(cwd: &PathBuf, log_dir: &PathBuf, args: I, label: &str) -> CmdOutput
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("br"));
    cmd.current_dir(cwd);
    cmd.args(args);
    cmd.env("NO_COLOR", "1");
    cmd.env("RUST_LOG", "beads_rust=debug");
    cmd.env("RUST_BACKTRACE", "1");
    cmd.env("HOME", cwd);

    run_and_log(cmd, cwd, log_dir, label)
}

fn run_bd_cmd<I, S>(cwd: &PathBuf, log_dir: &PathBuf, args: I, label: &str) -> CmdOutput
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_cmd_system("bd", cwd, log_dir, args, label)
}

fn run_cmd_system<I, S>(
    binary: &str,
    cwd: &PathBuf,
    log_dir: &PathBuf,
    args: I,
    label: &str,
) -> CmdOutput
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = std::process::Command::new(binary);
    cmd.current_dir(cwd);
    cmd.args(args);
    cmd.env("NO_COLOR", "1");
    cmd.env("HOME", cwd);

    let start = Instant::now();
    let output = cmd.output().expect(&format!("run {binary}"));
    let duration = start.elapsed();

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // Log output
    let log_path = log_dir.join(format!("{label}.log"));
    let timestamp = SystemTime::now();
    let log_body = format!(
        "label: {label}\nbinary: {binary}\nstarted: {:?}\nduration: {:?}\nstatus: {}\ncwd: {}\n\nstdout:\n{}\n\nstderr:\n{}\n",
        timestamp,
        duration,
        output.status,
        cwd.display(),
        stdout,
        stderr
    );
    fs::write(&log_path, log_body).expect("write log");

    CmdOutput {
        stdout,
        stderr,
        status: output.status,
        duration,
    }
}

fn run_and_log(mut cmd: Command, cwd: &PathBuf, log_dir: &PathBuf, label: &str) -> CmdOutput {
    let start = Instant::now();
    let output = cmd.output().expect("run command");
    let duration = start.elapsed();

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let log_path = log_dir.join(format!("{label}.log"));
    let timestamp = SystemTime::now();
    let log_body = format!(
        "label: {label}\nstarted: {:?}\nduration: {:?}\nstatus: {}\nargs: {:?}\ncwd: {}\n\nstdout:\n{}\n\nstderr:\n{}\n",
        timestamp,
        duration,
        output.status,
        cmd.get_args().collect::<Vec<_>>(),
        cwd.display(),
        stdout,
        stderr
    );
    fs::write(&log_path, log_body).expect("write log");

    CmdOutput {
        stdout,
        stderr,
        status: output.status,
        duration,
    }
}

/// Comparison mode for conformance tests
#[derive(Debug, Clone)]
pub enum CompareMode {
    /// JSON outputs must be identical
    ExactJson,
    /// Ignore timestamps and normalize IDs
    NormalizedJson,
    /// Check specific fields match
    ContainsFields(Vec<String>),
    /// Just check that both succeed or both fail
    ExitCodeOnly,
}

/// Normalize JSON for comparison by removing/masking volatile fields
pub fn normalize_json(json_str: &str) -> Result<Value, serde_json::Error> {
    let mut value: Value = serde_json::from_str(json_str)?;
    normalize_value(&mut value);
    Ok(value)
}

fn normalize_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            // Fields to normalize (set to fixed values)
            let timestamp_fields: HashSet<&str> = [
                "created_at",
                "updated_at",
                "closed_at",
                "deleted_at",
                "due_at",
                "defer_until",
                "compacted_at",
            ]
            .into_iter()
            .collect();

            // Normalize timestamps to a fixed value
            for (key, val) in map.iter_mut() {
                if timestamp_fields.contains(key.as_str()) {
                    if val.is_string() {
                        *val = Value::String("NORMALIZED_TIMESTAMP".to_string());
                    }
                } else if key == "id" || key == "issue_id" || key == "depends_on_id" {
                    // Keep ID structure but normalize the hash portion
                    if let Some(s) = val.as_str() {
                        if let Some(dash_pos) = s.find('-') {
                            let prefix = &s[..dash_pos];
                            *val = Value::String(format!("{prefix}-NORMALIZED"));
                        }
                    }
                } else if key == "content_hash" {
                    if val.is_string() {
                        *val = Value::String("NORMALIZED_HASH".to_string());
                    }
                } else {
                    normalize_value(val);
                }
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                normalize_value(item);
            }
        }
        _ => {}
    }
}

/// Compare two JSON outputs
pub fn compare_json(br_output: &str, bd_output: &str, mode: &CompareMode) -> Result<(), String> {
    match mode {
        CompareMode::ExactJson => {
            let br_json: Value =
                serde_json::from_str(br_output).map_err(|e| format!("br JSON parse: {e}"))?;
            let bd_json: Value =
                serde_json::from_str(bd_output).map_err(|e| format!("bd JSON parse: {e}"))?;

            if br_json != bd_json {
                return Err(format!(
                    "JSON mismatch\nbr: {}\nbd: {}",
                    serde_json::to_string_pretty(&br_json).unwrap_or_default(),
                    serde_json::to_string_pretty(&bd_json).unwrap_or_default()
                ));
            }
        }
        CompareMode::NormalizedJson => {
            let br_json = normalize_json(br_output).map_err(|e| format!("br JSON parse: {e}"))?;
            let bd_json = normalize_json(bd_output).map_err(|e| format!("bd JSON parse: {e}"))?;

            if br_json != bd_json {
                return Err(format!(
                    "Normalized JSON mismatch\nbr: {}\nbd: {}",
                    serde_json::to_string_pretty(&br_json).unwrap_or_default(),
                    serde_json::to_string_pretty(&bd_json).unwrap_or_default()
                ));
            }
        }
        CompareMode::ContainsFields(fields) => {
            let br_json: Value =
                serde_json::from_str(br_output).map_err(|e| format!("br JSON parse: {e}"))?;
            let bd_json: Value =
                serde_json::from_str(bd_output).map_err(|e| format!("bd JSON parse: {e}"))?;

            for field in fields {
                let br_val = extract_field(&br_json, field);
                let bd_val = extract_field(&bd_json, field);

                if br_val != bd_val {
                    return Err(format!(
                        "Field '{}' mismatch\nbr: {:?}\nbd: {:?}",
                        field, br_val, bd_val
                    ));
                }
            }
        }
        CompareMode::ExitCodeOnly => {
            // No JSON comparison needed
        }
    }
    Ok(())
}

fn extract_field<'a>(json: &'a Value, field: &str) -> Option<&'a Value> {
    match json {
        Value::Object(map) => map.get(field),
        Value::Array(arr) if !arr.is_empty() => {
            // For arrays, check the first element
            if let Value::Object(map) = &arr[0] {
                map.get(field)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Helper to parse the created issue ID from br/bd create output
fn parse_created_id(stdout: &str) -> Option<String> {
    // br format: "Created bd-xxx: Title"
    // bd format: "Created bd-xxx: Title" or similar
    let line = stdout.lines().next()?;
    let after_created = line.strip_prefix("Created ")?;
    let id = after_created.split(':').next()?;
    Some(id.trim().to_string())
}

// ============================================================================
// CONFORMANCE TESTS
// ============================================================================

#[test]
fn conformance_init() {
    common::init_test_logging();
    info!("Starting conformance_init test");

    let workspace = ConformanceWorkspace::new();
    let (br_out, bd_out) = workspace.init_both();

    assert!(br_out.status.success(), "br init failed: {}", br_out.stderr);
    assert!(bd_out.status.success(), "bd init failed: {}", bd_out.stderr);

    // Both should create .beads directories
    assert!(
        workspace.br_root.join(".beads").exists(),
        "br did not create .beads"
    );
    assert!(
        workspace.bd_root.join(".beads").exists(),
        "bd did not create .beads"
    );

    info!("conformance_init passed");
}

#[test]
fn conformance_create_basic() {
    common::init_test_logging();
    info!("Starting conformance_create_basic test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues with same parameters
    let br_create = workspace.run_br(["create", "Test issue", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Test issue", "--json"], "create");

    assert!(
        br_create.status.success(),
        "br create failed: {}",
        br_create.stderr
    );
    assert!(
        bd_create.status.success(),
        "bd create failed: {}",
        bd_create.stderr
    );

    // Compare with ContainsFields - title, status, priority should match
    let br_json = extract_json_payload(&br_create.stdout);
    let bd_json = extract_json_payload(&bd_create.stdout);

    let result = compare_json(
        &br_json,
        &bd_json,
        &CompareMode::ContainsFields(vec![
            "title".to_string(),
            "status".to_string(),
            "issue_type".to_string(),
        ]),
    );

    assert!(result.is_ok(), "JSON comparison failed: {:?}", result.err());
    info!("conformance_create_basic passed");
}

#[test]
fn conformance_create_with_type_and_priority() {
    common::init_test_logging();
    info!("Starting conformance_create_with_type_and_priority test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let args = [
        "create",
        "Bug fix needed",
        "--type",
        "bug",
        "--priority",
        "1",
        "--json",
    ];

    let br_create = workspace.run_br(args.clone(), "create_bug");
    let bd_create = workspace.run_bd(args, "create_bug");

    assert!(
        br_create.status.success(),
        "br create failed: {}",
        br_create.stderr
    );
    assert!(
        bd_create.status.success(),
        "bd create failed: {}",
        bd_create.stderr
    );

    let br_json = extract_json_payload(&br_create.stdout);
    let bd_json = extract_json_payload(&bd_create.stdout);

    // Parse and verify specific fields
    let br_val: Value = serde_json::from_str(&br_json).expect("br json");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("bd json");

    // Handle both object and array outputs
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

    assert_eq!(br_issue["title"], bd_issue["title"], "title mismatch");
    assert_eq!(
        br_issue["issue_type"], bd_issue["issue_type"],
        "issue_type mismatch: br={}, bd={}",
        br_issue["issue_type"], bd_issue["issue_type"]
    );
    assert_eq!(
        br_issue["priority"], bd_issue["priority"],
        "priority mismatch: br={}, bd={}",
        br_issue["priority"], bd_issue["priority"]
    );

    info!("conformance_create_with_type_and_priority passed");
}

#[test]
fn conformance_list_empty() {
    common::init_test_logging();
    info!("Starting conformance_list_empty test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_list = workspace.run_br(["list", "--json"], "list_empty");
    let bd_list = workspace.run_bd(["list", "--json"], "list_empty");

    assert!(
        br_list.status.success(),
        "br list failed: {}",
        br_list.stderr
    );
    assert!(
        bd_list.status.success(),
        "bd list failed: {}",
        bd_list.stderr
    );

    // Both should return empty arrays
    let br_json = extract_json_payload(&br_list.stdout);
    let bd_json = extract_json_payload(&bd_list.stdout);

    let br_val: Value = serde_json::from_str(&br_json).unwrap_or(Value::Null);
    let bd_val: Value = serde_json::from_str(&bd_json).unwrap_or(Value::Null);

    // Both should be empty arrays or similar
    let br_len = br_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_len = bd_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_len, bd_len,
        "list lengths differ: br={}, bd={}",
        br_len, bd_len
    );
    assert_eq!(br_len, 0, "expected empty list");

    info!("conformance_list_empty passed");
}

#[test]
fn conformance_list_with_issues() {
    common::init_test_logging();
    info!("Starting conformance_list_with_issues test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create same issues in both
    workspace.run_br(["create", "Issue one"], "create1");
    workspace.run_bd(["create", "Issue one"], "create1");

    workspace.run_br(["create", "Issue two"], "create2");
    workspace.run_bd(["create", "Issue two"], "create2");

    let br_list = workspace.run_br(["list", "--json"], "list");
    let bd_list = workspace.run_bd(["list", "--json"], "list");

    assert!(
        br_list.status.success(),
        "br list failed: {}",
        br_list.stderr
    );
    assert!(
        bd_list.status.success(),
        "bd list failed: {}",
        bd_list.stderr
    );

    let br_json = extract_json_payload(&br_list.stdout);
    let bd_json = extract_json_payload(&bd_list.stdout);

    let br_val: Value = serde_json::from_str(&br_json).expect("br json");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("bd json");

    let br_len = br_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_len = bd_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_len, bd_len,
        "list lengths differ: br={}, bd={}",
        br_len, bd_len
    );
    assert_eq!(br_len, 2, "expected 2 issues");

    info!("conformance_list_with_issues passed");
}

#[test]
fn conformance_ready_empty() {
    common::init_test_logging();
    info!("Starting conformance_ready_empty test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_ready = workspace.run_br(["ready", "--json"], "ready_empty");
    let bd_ready = workspace.run_bd(["ready", "--json"], "ready_empty");

    assert!(
        br_ready.status.success(),
        "br ready failed: {}",
        br_ready.stderr
    );
    assert!(
        bd_ready.status.success(),
        "bd ready failed: {}",
        bd_ready.stderr
    );

    let br_json = extract_json_payload(&br_ready.stdout);
    let bd_json = extract_json_payload(&bd_ready.stdout);

    let br_val: Value = serde_json::from_str(&br_json).unwrap_or(Value::Array(vec![]));
    let bd_val: Value = serde_json::from_str(&bd_json).unwrap_or(Value::Array(vec![]));

    let br_len = br_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_len = bd_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_len, bd_len,
        "ready lengths differ: br={}, bd={}",
        br_len, bd_len
    );

    info!("conformance_ready_empty passed");
}

#[test]
fn conformance_ready_with_issues() {
    common::init_test_logging();
    info!("Starting conformance_ready_with_issues test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues
    workspace.run_br(["create", "Ready issue"], "create");
    workspace.run_bd(["create", "Ready issue"], "create");

    let br_ready = workspace.run_br(["ready", "--json"], "ready");
    let bd_ready = workspace.run_bd(["ready", "--json"], "ready");

    assert!(
        br_ready.status.success(),
        "br ready failed: {}",
        br_ready.stderr
    );
    assert!(
        bd_ready.status.success(),
        "bd ready failed: {}",
        bd_ready.stderr
    );

    let br_json = extract_json_payload(&br_ready.stdout);
    let bd_json = extract_json_payload(&bd_ready.stdout);

    let br_val: Value = serde_json::from_str(&br_json).expect("br json");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("bd json");

    let br_len = br_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_len = bd_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_len, bd_len,
        "ready lengths differ: br={}, bd={}",
        br_len, bd_len
    );
    assert_eq!(br_len, 1, "expected 1 ready issue");

    info!("conformance_ready_with_issues passed");
}

#[test]
fn conformance_blocked_empty() {
    common::init_test_logging();
    info!("Starting conformance_blocked_empty test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_blocked = workspace.run_br(["blocked", "--json"], "blocked_empty");
    let bd_blocked = workspace.run_bd(["blocked", "--json"], "blocked_empty");

    assert!(
        br_blocked.status.success(),
        "br blocked failed: {}",
        br_blocked.stderr
    );
    assert!(
        bd_blocked.status.success(),
        "bd blocked failed: {}",
        bd_blocked.stderr
    );

    let br_json = extract_json_payload(&br_blocked.stdout);
    let bd_json = extract_json_payload(&bd_blocked.stdout);

    let br_val: Value = serde_json::from_str(&br_json).unwrap_or(Value::Array(vec![]));
    let bd_val: Value = serde_json::from_str(&bd_json).unwrap_or(Value::Array(vec![]));

    let br_len = br_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_len = bd_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(br_len, bd_len, "blocked lengths differ");
    assert_eq!(br_len, 0, "expected no blocked issues");

    info!("conformance_blocked_empty passed");
}

#[test]
fn conformance_stats() {
    common::init_test_logging();
    info!("Starting conformance_stats test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create some issues to have stats
    workspace.run_br(["create", "Issue A"], "create_a");
    workspace.run_bd(["create", "Issue A"], "create_a");

    let br_stats = workspace.run_br(["stats", "--json"], "stats");
    let bd_stats = workspace.run_bd(["stats", "--json"], "stats");

    assert!(
        br_stats.status.success(),
        "br stats failed: {}",
        br_stats.stderr
    );
    assert!(
        bd_stats.status.success(),
        "bd stats failed: {}",
        bd_stats.stderr
    );

    // Stats command returns structured data - verify key fields match
    let br_json = extract_json_payload(&br_stats.stdout);
    let bd_json = extract_json_payload(&bd_stats.stdout);

    let br_val: Value = serde_json::from_str(&br_json).expect("br json");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("bd json");

    // Both should report same total count
    let br_total = br_val["total"]
        .as_i64()
        .or_else(|| br_val["summary"]["total"].as_i64());
    let bd_total = bd_val["total"]
        .as_i64()
        .or_else(|| bd_val["summary"]["total"].as_i64());

    assert_eq!(
        br_total, bd_total,
        "total issue counts differ: br={:?}, bd={:?}",
        br_total, bd_total
    );

    info!("conformance_stats passed");
}

#[test]
fn conformance_sync_flush_only() {
    common::init_test_logging();
    info!("Starting conformance_sync_flush_only test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues
    workspace.run_br(["create", "Sync test issue"], "create");
    workspace.run_bd(["create", "Sync test issue"], "create");

    // Run sync --flush-only
    let br_sync = workspace.run_br(["sync", "--flush-only"], "sync");
    let bd_sync = workspace.run_bd(["sync", "--flush-only"], "sync");

    assert!(
        br_sync.status.success(),
        "br sync failed: {}",
        br_sync.stderr
    );
    assert!(
        bd_sync.status.success(),
        "bd sync failed: {}",
        bd_sync.stderr
    );

    // Both should create issues.jsonl
    let br_jsonl = workspace.br_root.join(".beads").join("issues.jsonl");
    let bd_jsonl = workspace.bd_root.join(".beads").join("issues.jsonl");

    assert!(br_jsonl.exists(), "br did not create issues.jsonl");
    assert!(bd_jsonl.exists(), "bd did not create issues.jsonl");

    // Verify JSONL files are non-empty
    let br_content = fs::read_to_string(&br_jsonl).expect("read br jsonl");
    let bd_content = fs::read_to_string(&bd_jsonl).expect("read bd jsonl");

    assert!(!br_content.trim().is_empty(), "br issues.jsonl is empty");
    assert!(!bd_content.trim().is_empty(), "bd issues.jsonl is empty");

    // Both should have exactly 1 line (1 issue)
    let br_lines = br_content.lines().count();
    let bd_lines = bd_content.lines().count();

    assert_eq!(
        br_lines, bd_lines,
        "JSONL line counts differ: br={}, bd={}",
        br_lines, bd_lines
    );

    info!("conformance_sync_flush_only passed");
}

#[test]
fn conformance_dependency_blocking() {
    common::init_test_logging();
    info!("Starting conformance_dependency_blocking test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create blocker and blocked issues
    let br_blocker = workspace.run_br(["create", "Blocker issue", "--json"], "create_blocker");
    let bd_blocker = workspace.run_bd(["create", "Blocker issue", "--json"], "create_blocker");

    let br_blocked = workspace.run_br(["create", "Blocked issue", "--json"], "create_blocked");
    let bd_blocked = workspace.run_bd(["create", "Blocked issue", "--json"], "create_blocked");

    // Extract IDs
    let br_blocker_json = extract_json_payload(&br_blocker.stdout);
    let bd_blocker_json = extract_json_payload(&bd_blocker.stdout);
    let br_blocked_json = extract_json_payload(&br_blocked.stdout);
    let bd_blocked_json = extract_json_payload(&bd_blocked.stdout);

    let br_blocker_val: Value = serde_json::from_str(&br_blocker_json).expect("parse");
    let bd_blocker_val: Value = serde_json::from_str(&bd_blocker_json).expect("parse");
    let br_blocked_val: Value = serde_json::from_str(&br_blocked_json).expect("parse");
    let bd_blocked_val: Value = serde_json::from_str(&bd_blocked_json).expect("parse");

    let br_blocker_id = br_blocker_val["id"]
        .as_str()
        .or_else(|| br_blocker_val[0]["id"].as_str())
        .unwrap();
    let bd_blocker_id = bd_blocker_val["id"]
        .as_str()
        .or_else(|| bd_blocker_val[0]["id"].as_str())
        .unwrap();
    let br_blocked_id = br_blocked_val["id"]
        .as_str()
        .or_else(|| br_blocked_val[0]["id"].as_str())
        .unwrap();
    let bd_blocked_id = bd_blocked_val["id"]
        .as_str()
        .or_else(|| bd_blocked_val[0]["id"].as_str())
        .unwrap();

    // Add dependency: blocked depends on blocker
    let br_dep = workspace.run_br(["dep", "add", br_blocked_id, br_blocker_id], "add_dep");
    let bd_dep = workspace.run_bd(["dep", "add", bd_blocked_id, bd_blocker_id], "add_dep");

    assert!(
        br_dep.status.success(),
        "br dep add failed: {}",
        br_dep.stderr
    );
    assert!(
        bd_dep.status.success(),
        "bd dep add failed: {}",
        bd_dep.stderr
    );

    // Check blocked command
    let br_blocked_cmd = workspace.run_br(["blocked", "--json"], "blocked");
    let bd_blocked_cmd = workspace.run_bd(["blocked", "--json"], "blocked");

    assert!(br_blocked_cmd.status.success(), "br blocked failed");
    assert!(bd_blocked_cmd.status.success(), "bd blocked failed");

    let br_blocked_json = extract_json_payload(&br_blocked_cmd.stdout);
    let bd_blocked_json = extract_json_payload(&bd_blocked_cmd.stdout);

    let br_val: Value = serde_json::from_str(&br_blocked_json).unwrap_or(Value::Array(vec![]));
    let bd_val: Value = serde_json::from_str(&bd_blocked_json).unwrap_or(Value::Array(vec![]));

    let br_len = br_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_len = bd_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_len, bd_len,
        "blocked counts differ: br={}, bd={}",
        br_len, bd_len
    );
    assert_eq!(br_len, 1, "expected 1 blocked issue");

    // Check ready - should only show the blocker, not the blocked issue
    let br_ready = workspace.run_br(["ready", "--json"], "ready_after_dep");
    let bd_ready = workspace.run_bd(["ready", "--json"], "ready_after_dep");

    let br_ready_json = extract_json_payload(&br_ready.stdout);
    let bd_ready_json = extract_json_payload(&bd_ready.stdout);

    let br_ready_val: Value = serde_json::from_str(&br_ready_json).unwrap_or(Value::Array(vec![]));
    let bd_ready_val: Value = serde_json::from_str(&bd_ready_json).unwrap_or(Value::Array(vec![]));

    let br_ready_len = br_ready_val.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_ready_len = bd_ready_val.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_ready_len, bd_ready_len,
        "ready counts differ: br={}, bd={}",
        br_ready_len, bd_ready_len
    );
    assert_eq!(br_ready_len, 1, "expected 1 ready issue (the blocker)");

    info!("conformance_dependency_blocking passed");
}

#[test]
fn conformance_close_issue() {
    common::init_test_logging();
    info!("Starting conformance_close_issue test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues
    let br_create = workspace.run_br(["create", "Issue to close", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Issue to close", "--json"], "create");

    let br_json = extract_json_payload(&br_create.stdout);
    let bd_json = extract_json_payload(&bd_create.stdout);

    let br_val: Value = serde_json::from_str(&br_json).expect("parse");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("parse");

    let br_id = br_val["id"]
        .as_str()
        .or_else(|| br_val[0]["id"].as_str())
        .unwrap();
    let bd_id = bd_val["id"]
        .as_str()
        .or_else(|| bd_val[0]["id"].as_str())
        .unwrap();

    // Close issues
    let br_close = workspace.run_br(["close", br_id, "--json"], "close");
    let bd_close = workspace.run_bd(["close", bd_id, "--json"], "close");

    assert!(
        br_close.status.success(),
        "br close failed: {}",
        br_close.stderr
    );
    assert!(
        bd_close.status.success(),
        "bd close failed: {}",
        bd_close.stderr
    );

    // Verify via show that issues are closed (list may exclude closed by default)
    let br_show = workspace.run_br(["show", br_id, "--json"], "show_after_close");
    let bd_show = workspace.run_bd(["show", bd_id, "--json"], "show_after_close");

    assert!(
        br_show.status.success(),
        "br show failed: {}",
        br_show.stderr
    );
    assert!(
        bd_show.status.success(),
        "bd show failed: {}",
        bd_show.stderr
    );

    let br_show_json = extract_json_payload(&br_show.stdout);
    let bd_show_json = extract_json_payload(&bd_show.stdout);

    let br_show_val: Value = serde_json::from_str(&br_show_json).expect("parse");
    let bd_show_val: Value = serde_json::from_str(&bd_show_json).expect("parse");

    // Handle array or object response
    let br_issue = if br_show_val.is_array() {
        &br_show_val[0]
    } else {
        &br_show_val
    };
    let bd_issue = if bd_show_val.is_array() {
        &bd_show_val[0]
    } else {
        &bd_show_val
    };

    assert_eq!(
        br_issue["status"].as_str(),
        Some("closed"),
        "br issue not closed: got {:?}",
        br_issue["status"]
    );
    assert_eq!(
        bd_issue["status"].as_str(),
        Some("closed"),
        "bd issue not closed: got {:?}",
        bd_issue["status"]
    );

    info!("conformance_close_issue passed");
}

#[test]
fn conformance_update_issue() {
    common::init_test_logging();
    info!("Starting conformance_update_issue test");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issues
    let br_create = workspace.run_br(["create", "Issue to update", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Issue to update", "--json"], "create");

    let br_json = extract_json_payload(&br_create.stdout);
    let bd_json = extract_json_payload(&bd_create.stdout);

    let br_val: Value = serde_json::from_str(&br_json).expect("parse");
    let bd_val: Value = serde_json::from_str(&bd_json).expect("parse");

    let br_id = br_val["id"]
        .as_str()
        .or_else(|| br_val[0]["id"].as_str())
        .unwrap();
    let bd_id = bd_val["id"]
        .as_str()
        .or_else(|| bd_val[0]["id"].as_str())
        .unwrap();

    // Update priority
    let br_update = workspace.run_br(
        ["update", br_id, "--priority", "0", "--json"],
        "update_priority",
    );
    let bd_update = workspace.run_bd(
        ["update", bd_id, "--priority", "0", "--json"],
        "update_priority",
    );

    assert!(
        br_update.status.success(),
        "br update failed: {}",
        br_update.stderr
    );
    assert!(
        bd_update.status.success(),
        "bd update failed: {}",
        bd_update.stderr
    );

    // Verify via show
    let br_show = workspace.run_br(["show", br_id, "--json"], "show_after_update");
    let bd_show = workspace.run_bd(["show", bd_id, "--json"], "show_after_update");

    let br_show_json = extract_json_payload(&br_show.stdout);
    let bd_show_json = extract_json_payload(&bd_show.stdout);

    let br_show_val: Value = serde_json::from_str(&br_show_json).expect("parse");
    let bd_show_val: Value = serde_json::from_str(&bd_show_json).expect("parse");

    let br_priority = br_show_val["priority"]
        .as_i64()
        .or_else(|| br_show_val[0]["priority"].as_i64());
    let bd_priority = bd_show_val["priority"]
        .as_i64()
        .or_else(|| bd_show_val[0]["priority"].as_i64());

    assert_eq!(
        br_priority, bd_priority,
        "priority mismatch after update: br={:?}, bd={:?}",
        br_priority, bd_priority
    );
    assert_eq!(br_priority, Some(0), "expected priority 0");

    info!("conformance_update_issue passed");
}
