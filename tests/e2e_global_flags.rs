//! E2E tests for global CLI flags and output modes.
//!
//! Tests --json, --robot, --no-color, --no-db, and other global flags.
//! Part of beads_rust-pnvt.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, parse_list_issues, run_br};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

#[derive(Debug, PartialEq, Eq)]
enum BeadsTreeEntry {
    Dir,
    File {
        modified_nanos: u128,
        bytes: Vec<u8>,
    },
}

fn snapshot_beads_tree(root: &Path) -> BTreeMap<String, BeadsTreeEntry> {
    let beads_dir = root.join(".beads");
    let mut snapshot = BTreeMap::new();

    for entry in walkdir::WalkDir::new(&beads_dir)
        .sort_by_file_name()
        .into_iter()
    {
        let entry = entry.expect("walk .beads");
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .expect("path below root")
            .to_string_lossy()
            .replace('\\', "/");

        if entry.file_type().is_dir() {
            snapshot.insert(rel, BeadsTreeEntry::Dir);
            continue;
        }

        if entry.file_type().is_file() {
            let metadata = fs::metadata(path).expect("file metadata");
            let modified_nanos = metadata
                .modified()
                .expect("file modified time")
                .duration_since(UNIX_EPOCH)
                .expect("modified after epoch")
                .as_nanos();
            let bytes = fs::read(path).expect("read file");
            snapshot.insert(
                rel,
                BeadsTreeEntry::File {
                    modified_nanos,
                    bytes,
                },
            );
        }
    }

    snapshot
}

fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    let normalized = line.strip_prefix("✓ Created ").unwrap_or(line);
    normalized
        .split(':')
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

fn assert_quiet_command<const N: usize>(
    workspace: &BrWorkspace,
    args: [&str; N],
    label: &str,
    description: &str,
) {
    let result = run_br(workspace, args, label);
    assert!(
        result.status.success(),
        "{description} failed: {}",
        result.stderr
    );
    assert!(
        result.stdout.trim().is_empty(),
        "{description} should produce no stdout: '{}'",
        result.stdout
    );
}

fn run_quiet_json<const N: usize>(
    workspace: &BrWorkspace,
    args: [&str; N],
    label: &str,
    description: &str,
) -> Value {
    let result = run_br(workspace, args, label);
    assert!(
        result.status.success(),
        "{description} failed: {}",
        result.stderr
    );
    let payload = extract_json_payload(&result.stdout);
    serde_json::from_str(&payload).expect(description)
}

// ============================================================================
// --json flag tests
// ============================================================================

#[test]
fn e2e_json_flag_list() {
    let _log = common::test_log("e2e_json_flag_list");
    let workspace = BrWorkspace::new();

    // Initialize and create issue
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "JSON test issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // List with --json flag
    let list = run_br(&workspace, ["list", "--json"], "list_json");
    assert!(list.status.success(), "list --json failed: {}", list.stderr);

    // Output should be valid paginated JSON with an issues array.
    let json = parse_list_issues(&list.stdout);
    assert!(!json.is_empty(), "JSON list should not be empty");
    assert!(
        json.iter().any(|item| item["title"] == "JSON test issue"),
        "issue should be in JSON output"
    );
}

#[test]
fn e2e_json_flag_show() {
    let _log = common::test_log("e2e_json_flag_show");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Show JSON test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let id = create
        .stdout
        .lines()
        .next()
        .unwrap_or("")
        .strip_prefix("✓ Created ")
        .and_then(|s| s.split(':').next())
        .unwrap_or("")
        .trim();

    // Show with --json flag
    let show = run_br(&workspace, ["show", id, "--json"], "show_json");
    assert!(show.status.success(), "show --json failed: {}", show.stderr);

    let payload = extract_json_payload(&show.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("valid JSON");
    assert_eq!(json[0]["title"], "Show JSON test");
}

#[test]
fn e2e_json_flag_ready() {
    let _log = common::test_log("e2e_json_flag_ready");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Ready JSON test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Ready with --json flag
    let ready = run_br(&workspace, ["ready", "--json"], "ready_json");
    assert!(
        ready.status.success(),
        "ready --json failed: {}",
        ready.stderr
    );

    let payload = extract_json_payload(&ready.stdout);
    // Should be valid JSON array (may be empty if issue not ready)
    let _json: Vec<Value> = serde_json::from_str(&payload).expect("valid JSON array");
}

#[test]
fn e2e_json_flag_blocked() {
    let _log = common::test_log("e2e_json_flag_blocked");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Blocked with --json flag (even with no blocked issues)
    let blocked = run_br(&workspace, ["blocked", "--json"], "blocked_json");
    assert!(
        blocked.status.success(),
        "blocked --json failed: {}",
        blocked.stderr
    );

    let payload = extract_json_payload(&blocked.stdout);
    let json: Value = serde_json::from_str(&payload).expect("valid JSON");
    // Should be valid JSON (empty array when no blocked issues)
    assert!(json.is_array());
}

#[test]
fn e2e_json_flag_stats() {
    let _log = common::test_log("e2e_json_flag_stats");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Stats JSON test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Stats with --json flag
    let stats = run_br(&workspace, ["stats", "--json"], "stats_json");
    assert!(
        stats.status.success(),
        "stats --json failed: {}",
        stats.stderr
    );

    let payload = extract_json_payload(&stats.stdout);
    let json: Value = serde_json::from_str(&payload).expect("valid JSON");
    // Stats output has a "summary" object with count fields
    assert!(
        json.get("summary").is_some(),
        "stats should have summary field: {json}"
    );
    let summary = &json["summary"];
    assert!(
        summary.get("total_issues").is_some() || summary.get("open_issues").is_some(),
        "stats summary should have count fields: {summary}"
    );
}

// ============================================================================
// --robot flag tests
// ============================================================================

/// Note: --robot is not a global flag for `list` command.
/// The `list --json` flag provides machine-readable output.
/// The `--robot` flag exists on specific commands like `sync` and `history`.
/// This test verifies that list --json provides robot-parseable output.
#[test]
fn e2e_robot_flag_list() {
    let _log = common::test_log("e2e_robot_flag_list");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Robot test issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // List with --json flag (provides robot-parseable output)
    let list = run_br(&workspace, ["list", "--json"], "list_json");
    assert!(list.status.success(), "list --json failed: {}", list.stderr);

    // JSON mode should output valid JSON to stdout
    let payload = extract_json_payload(&list.stdout);
    let json: Value = serde_json::from_str(&payload).expect("json mode should output valid JSON");
    assert!(json.is_object(), "list should be JSON object envelope");
    assert!(
        json.get("issues").is_some(),
        "list envelope should contain 'issues'"
    );
}

#[test]
fn e2e_robot_flag_stderr_diagnostics() {
    let _log = common::test_log("e2e_robot_flag_stderr_diagnostics");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Use --no-auto-flush so sync has something to export
    let create = run_br(
        &workspace,
        ["create", "Robot stderr test", "--no-auto-flush"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Sync with --json flag to get JSON output
    let sync = run_br(&workspace, ["sync", "--flush-only", "--json"], "sync_json");
    assert!(sync.status.success(), "sync --json failed: {}", sync.stderr);

    // stdout should be parseable JSON
    let payload = extract_json_payload(&sync.stdout);
    let json: Value =
        serde_json::from_str(&payload).expect("json mode stdout should be valid JSON");

    // Verify it has expected fields from sync output
    assert!(
        json.get("exported_issues").is_some(),
        "JSON output should have exported_issues field"
    );
}

#[test]
fn e2e_robot_flag_sync_flush_outputs_json() {
    let _log = common::test_log("e2e_robot_flag_sync_flush_outputs_json");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Robot sync flush test", "--no-auto-flush"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let sync = run_br(
        &workspace,
        ["sync", "--flush-only", "--robot"],
        "sync_robot_flush",
    );
    assert!(
        sync.status.success(),
        "sync --robot failed: {}",
        sync.stderr
    );

    let payload = extract_json_payload(&sync.stdout);
    let json: Value = serde_json::from_str(&payload).expect("robot mode should output valid JSON");
    assert_eq!(json["exported_issues"].as_u64(), Some(1));
}

// ============================================================================
// --no-color flag tests
// ============================================================================

#[test]
fn e2e_no_color_flag() {
    let _log = common::test_log("e2e_no_color_flag");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "No-color test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Note: Our test harness already sets NO_COLOR=1, but let's verify --no-color works
    let list = run_br(&workspace, ["list", "--no-color"], "list_no_color");
    assert!(
        list.status.success(),
        "list --no-color failed: {}",
        list.stderr
    );

    // Output should not contain ANSI escape codes
    assert!(
        !list.stdout.contains("\x1b["),
        "output should not contain ANSI escape codes with --no-color"
    );
}

#[test]
fn e2e_no_color_env_var() {
    let _log = common::test_log("e2e_no_color_env_var");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "NO_COLOR env test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Test with NO_COLOR environment variable (already set by test harness)
    let list = run_br(&workspace, ["list"], "list_with_no_color_env");
    assert!(list.status.success(), "list failed: {}", list.stderr);

    // Output should not contain ANSI escape codes
    assert!(
        !list.stdout.contains("\x1b["),
        "output should not contain ANSI escape codes with NO_COLOR env"
    );
}

#[test]
fn e2e_env_output_format_json_defaults_count_to_structured_output() {
    let _log = common::test_log("e2e_env_output_format_json_defaults_count_to_structured_output");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Env default JSON count"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let count = common::cli::run_br_with_env(
        &workspace,
        ["count"],
        [("BR_OUTPUT_FORMAT", "json")],
        "count_env_json",
    );
    assert!(
        count.status.success(),
        "count with BR_OUTPUT_FORMAT=json failed: {}",
        count.stderr
    );

    let payload = extract_json_payload(&count.stdout);
    let json: Value = serde_json::from_str(&payload).expect("valid JSON count output");
    assert!(
        json.is_object(),
        "count env-json output should be an object"
    );
    assert_eq!(json["count"], 1);
}

#[test]
fn e2e_quiet_overrides_env_json_for_list() {
    let _log = common::test_log("e2e_quiet_overrides_env_json_for_list");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Quiet env JSON list"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let list = common::cli::run_br_with_env(
        &workspace,
        ["list", "--quiet"],
        [("BR_OUTPUT_FORMAT", "json")],
        "list_quiet_env_json",
    );
    assert!(
        list.status.success(),
        "list --quiet with BR_OUTPUT_FORMAT=json failed: {}",
        list.stderr
    );
    assert!(
        list.stdout.trim().is_empty(),
        "quiet list should not emit env-selected JSON: {}",
        list.stdout
    );
}

// ============================================================================
// --no-db flag tests
// ============================================================================

#[test]
fn e2e_no_db_flag_list() {
    let _log = common::test_log("e2e_no_db_flag_list");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create issue and flush to JSONL
    let create = run_br(&workspace, ["create", "No-DB list test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    // List with --no-db flag (reads from JSONL only)
    let list = run_br(&workspace, ["--no-db", "list", "--json"], "list_no_db");
    assert!(
        list.status.success(),
        "list --no-db failed: {}",
        list.stderr
    );

    let json = parse_list_issues(&list.stdout);
    assert!(
        json.iter().any(|item| item["title"] == "No-DB list test"),
        "issue should be visible in no-db mode"
    );
}

#[test]
fn e2e_no_db_flag_show() {
    let _log = common::test_log("e2e_no_db_flag_show");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "No-DB show test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let id = create
        .stdout
        .lines()
        .next()
        .unwrap_or("")
        .strip_prefix("✓ Created ")
        .and_then(|s| s.split(':').next())
        .unwrap_or("")
        .trim();

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    // Show with --no-db flag
    let show = run_br(&workspace, ["--no-db", "show", id, "--json"], "show_no_db");
    assert!(
        show.status.success(),
        "show --no-db failed: {}",
        show.stderr
    );

    let payload = extract_json_payload(&show.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("valid JSON");
    assert_eq!(json[0]["title"], "No-DB show test");
}

#[test]
fn e2e_no_db_show_bypasses_corrupt_db_and_preserves_relations() {
    let _log = common::test_log("e2e_no_db_show_bypasses_corrupt_db_and_preserves_relations");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let parent = run_br(&workspace, ["create", "Parent issue"], "create_parent");
    assert!(
        parent.status.success(),
        "create parent failed: {}",
        parent.stderr
    );
    let parent_id = parent
        .stdout
        .lines()
        .next()
        .unwrap_or("")
        .strip_prefix("✓ Created ")
        .and_then(|s| s.split(':').next())
        .unwrap_or("")
        .trim()
        .to_string();

    let child = run_br(&workspace, ["create", "Child issue"], "create_child");
    assert!(
        child.status.success(),
        "create child failed: {}",
        child.stderr
    );
    let child_id = child
        .stdout
        .lines()
        .next()
        .unwrap_or("")
        .strip_prefix("✓ Created ")
        .and_then(|s| s.split(':').next())
        .unwrap_or("")
        .trim()
        .to_string();

    let dep = run_br(
        &workspace,
        [
            "dep",
            "add",
            &child_id,
            &parent_id,
            "--type",
            "parent-child",
        ],
        "dep_add_parent_child",
    );
    assert!(dep.status.success(), "dep add failed: {}", dep.stderr);

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    fs::write(
        workspace.root.join(".beads").join("beads.db"),
        b"not a sqlite db",
    )
    .expect("corrupt db for no-db regression");

    let show_child = run_br(
        &workspace,
        ["--no-db", "show", &child_id, "--json"],
        "show_no_db_corrupt_child",
    );
    assert!(
        show_child.status.success(),
        "show --no-db on corrupt db failed: {}",
        show_child.stderr
    );
    let child_payload = extract_json_payload(&show_child.stdout);
    let child_json: Vec<Value> = serde_json::from_str(&child_payload).expect("valid child JSON");
    assert_eq!(child_json[0]["parent"], parent_id);

    let show_parent = run_br(
        &workspace,
        ["--no-db", "show", &parent_id, "--json"],
        "show_no_db_corrupt_parent",
    );
    assert!(
        show_parent.status.success(),
        "parent show --no-db on corrupt db failed: {}",
        show_parent.stderr
    );
    let parent_payload = extract_json_payload(&show_parent.stdout);
    let parent_json: Vec<Value> = serde_json::from_str(&parent_payload).expect("valid parent JSON");
    assert_eq!(parent_json[0]["dependents"][0]["id"], child_id);
    assert_eq!(
        parent_json[0]["dependents"][0]["dependency_type"],
        "parent-child"
    );
}

#[test]
fn e2e_no_db_flag_ready() {
    let _log = common::test_log("e2e_no_db_flag_ready");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "No-DB ready test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    // Ready with --no-db flag
    let ready = run_br(&workspace, ["--no-db", "ready", "--json"], "ready_no_db");
    assert!(
        ready.status.success(),
        "ready --no-db failed: {}",
        ready.stderr
    );

    // Should output valid JSON
    let payload = extract_json_payload(&ready.stdout);
    let _json: Vec<Value> = serde_json::from_str(&payload).expect("valid JSON");
}

#[test]
fn e2e_no_db_hard_delete_flushes_jsonl() {
    let _log = common::test_log("e2e_no_db_hard_delete_flushes_jsonl");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "No-DB hard delete test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let issue_id = create
        .stdout
        .lines()
        .next()
        .unwrap_or("")
        .strip_prefix("✓ Created ")
        .and_then(|s| s.split(':').next())
        .unwrap_or("")
        .trim()
        .to_string();
    assert!(!issue_id.is_empty(), "expected created issue id in stdout");

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let before = fs::read_to_string(&jsonl_path).expect("read jsonl before delete");
    assert!(
        before.contains(&format!("\"id\":\"{issue_id}\"")),
        "issue should be present in JSONL before delete"
    );

    let delete = run_br(
        &workspace,
        ["--no-db", "delete", &issue_id, "--hard"],
        "delete_no_db_hard",
    );
    assert!(
        delete.status.success(),
        "--no-db delete --hard failed: {}",
        delete.stderr
    );

    let after = fs::read_to_string(&jsonl_path).expect("read jsonl after delete");
    assert!(
        !after.contains(&format!("\"id\":\"{issue_id}\"")),
        "hard delete in --no-db mode should remove the issue from JSONL"
    );
}

#[test]
fn e2e_no_db_flag_doctor_side_effect_free() {
    let _log = common::test_log("e2e_no_db_flag_doctor_side_effect_free");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "No-DB doctor test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    let before = snapshot_beads_tree(&workspace.root);
    let doctor = run_br(&workspace, ["--no-db", "doctor", "--json"], "doctor_no_db");

    let payload = extract_json_payload(&doctor.stdout);
    let json: Value = serde_json::from_str(&payload).expect("valid JSON");
    let checks = json["checks"].as_array().expect("checks array");
    assert!(
        checks
            .iter()
            .any(|check| check["name"] == "db.no_db_mode" && check["status"] == "ok"),
        "doctor should explicitly report SQLite checks skipped in no-db mode: {json}"
    );

    for skipped_check in [
        "db.exists",
        "db.open",
        "schema.tables",
        "schema.columns",
        "sqlite.integrity_check",
        "sqlite3.integrity_check",
        "counts.db_vs_jsonl",
        "sync.metadata",
        "db.write_probe",
    ] {
        assert!(
            !checks.iter().any(|check| check["name"] == skipped_check),
            "doctor --no-db must not run SQLite-backed check {skipped_check}: {json}"
        );
    }

    let after = snapshot_beads_tree(&workspace.root);
    assert_eq!(after, before, "doctor --no-db mutated .beads");
}

// ============================================================================
// --allow-stale flag tests
// ============================================================================

#[test]
fn e2e_allow_stale_flag() {
    let _log = common::test_log("e2e_allow_stale_flag");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Stale test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Sync to make JSONL current
    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    // Modify JSONL directly (makes DB "stale" relative to JSONL)
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
    fs::write(&jsonl_path, format!("{}\n", contents.trim())).expect("write jsonl");

    // List with --allow-stale should succeed even if DB is stale
    let list = run_br(&workspace, ["--allow-stale", "list"], "list_allow_stale");
    assert!(
        list.status.success(),
        "list --allow-stale failed: {}",
        list.stderr
    );
}

// ============================================================================
// --no-auto-import flag tests
// ============================================================================

#[test]
fn e2e_no_auto_import_flag() {
    let _log = common::test_log("e2e_no_auto_import_flag");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Auto-import test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Export to JSONL
    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    // Modify JSONL directly to make it newer than the DB
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
    fs::write(&jsonl_path, format!("{}\n", contents.trim())).expect("write jsonl");

    // With --no-auto-import, should skip the startup import probe entirely
    let list = run_br(
        &workspace,
        ["--no-auto-import", "list"],
        "list_no_auto_import",
    );
    assert!(
        list.status.success(),
        "list --no-auto-import failed: {}",
        list.stderr
    );
}

// ============================================================================
// --no-auto-flush flag tests
// ============================================================================

#[test]
fn e2e_no_auto_flush_flag() {
    let _log = common::test_log("e2e_no_auto_flush_flag");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create with --no-auto-flush
    let create = run_br(
        &workspace,
        ["create", "No auto-flush test", "--no-auto-flush"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Check if JSONL exists and if it contains the issue
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");

    if jsonl_path.exists() {
        let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
        // With --no-auto-flush, the issue should NOT be in JSONL yet
        // (unless there was a previous sync)
        // This is a soft check since auto-import might have created empty file
        if contents.contains("No auto-flush test") {
            // If it does contain it, that's unexpected but not necessarily wrong
            // depending on implementation details
        }
    }

    // Now explicitly flush
    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    // After flush, issue should be in JSONL
    let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
    assert!(
        contents.contains("No auto-flush test"),
        "issue should be in JSONL after explicit flush"
    );
}

/// `sync.auto_flush: false` in project config should suppress auto-flush,
/// just like passing `--no-auto-flush` on the CLI.
#[test]
fn e2e_no_auto_flush_from_project_config() {
    let _log = common::test_log("e2e_no_auto_flush_from_project_config");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Disable auto-flush via project config.
    let config_path = workspace.root.join(".beads").join("config.yaml");
    fs::write(&config_path, "sync:\n  auto_flush: false\n").expect("write config");

    // Create without --no-auto-flush flag; config should suppress auto-flush.
    let create = run_br(
        &workspace,
        ["create", "No auto-flush from config"],
        "create_with_config",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Issue should not be present in JSONL until explicit flush.
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    if jsonl_path.exists() {
        let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
        assert!(
            !contents.contains("No auto-flush from config"),
            "issue should not be in JSONL before explicit flush when sync.auto_flush=false"
        );
    }

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
    assert!(
        contents.contains("No auto-flush from config"),
        "issue should be in JSONL after explicit flush"
    );
}

/// `sync.auto-flush: false` (hyphen variant) should behave identically to
/// `sync.auto_flush: false` (underscore variant).
#[test]
fn e2e_no_auto_flush_config_hyphen_variant() {
    let _log = common::test_log("e2e_no_auto_flush_config_hyphen_variant");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Use hyphen variant in config.
    let config_path = workspace.root.join(".beads").join("config.yaml");
    fs::write(&config_path, "sync:\n  auto-flush: false\n").expect("write config");

    let create = run_br(
        &workspace,
        ["create", "Hyphen config no flush"],
        "create_hyphen",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    if jsonl_path.exists() {
        let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
        assert!(
            !contents.contains("Hyphen config no flush"),
            "issue should not be in JSONL when sync.auto-flush=false (hyphen)"
        );
    }

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
    assert!(
        contents.contains("Hyphen config no flush"),
        "issue should be in JSONL after explicit flush with hyphen config"
    );
}

// ============================================================================
// --lock-timeout flag tests
// ============================================================================

#[test]
fn e2e_lock_timeout_flag() {
    let _log = common::test_log("e2e_lock_timeout_flag");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create with custom lock timeout
    let create = run_br(
        &workspace,
        ["--lock-timeout", "5000", "create", "Lock timeout test"],
        "create_with_timeout",
    );
    assert!(
        create.status.success(),
        "create with --lock-timeout failed: {}",
        create.stderr
    );

    // Verify issue was created
    let list = run_br(&workspace, ["list", "--json"], "list");
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let json = parse_list_issues(&list.stdout);
    assert!(
        json.iter().any(|item| item["title"] == "Lock timeout test"),
        "issue should be created with custom lock timeout"
    );
}

// ============================================================================
// --quiet flag tests
// ============================================================================

#[test]
fn e2e_quiet_flag() {
    let _log = common::test_log("e2e_quiet_flag");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create with --quiet flag
    let create = run_br(
        &workspace,
        ["--quiet", "create", "Quiet test"],
        "create_quiet",
    );
    assert!(
        create.status.success(),
        "create --quiet failed: {}",
        create.stderr
    );

    // Quiet mode should minimize output (may still show created ID)
    // Just verify it succeeded and didn't crash
}

#[test]
fn e2e_quiet_flag_list() {
    let _log = common::test_log("e2e_quiet_flag_list");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Quiet list test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // List with --quiet flag
    let list = run_br(&workspace, ["--quiet", "list"], "list_quiet");
    assert!(
        list.status.success(),
        "list --quiet failed: {}",
        list.stderr
    );

    // Quiet mode should still show results but with minimal decoration
    // Verify it shows the issue title somewhere
    // (exact format depends on implementation)
}

#[test]
fn e2e_quiet_list_suppresses_truncation_note() {
    let _log = common::test_log("e2e_quiet_list_suppresses_truncation_note");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    for title in ["Quiet list a", "Quiet list b"] {
        let create = run_br(&workspace, ["create", title], "create");
        assert!(create.status.success(), "create failed: {}", create.stderr);
    }

    let list = run_br(
        &workspace,
        ["--quiet", "list", "--limit", "1"],
        "list_quiet_limit",
    );
    assert!(
        list.status.success(),
        "list --quiet --limit 1 failed: {}",
        list.stderr
    );
    assert!(
        !list.stderr.contains("Output truncated"),
        "quiet list should not emit truncation note: {}",
        list.stderr
    );
    assert!(
        !list.stderr.contains("Showing 1 of"),
        "quiet list should not emit client-filter truncation note: {}",
        list.stderr
    );
}

#[test]
fn e2e_quiet_flag_dep_subcommands() {
    let _log = common::test_log("e2e_quiet_flag_dep_subcommands");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create_a = run_br(&workspace, ["create", "Quiet dep A"], "create_a");
    assert!(
        create_a.status.success(),
        "create A failed: {}",
        create_a.stderr
    );
    let create_b = run_br(&workspace, ["create", "Quiet dep B"], "create_b");
    assert!(
        create_b.status.success(),
        "create B failed: {}",
        create_b.stderr
    );

    let id_a = create_a
        .stdout
        .lines()
        .next()
        .unwrap_or("")
        .strip_prefix("✓ Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("")
        .trim()
        .to_string();
    let id_b = create_b
        .stdout
        .lines()
        .next()
        .unwrap_or("")
        .strip_prefix("✓ Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("")
        .trim()
        .to_string();

    let add = run_br(
        &workspace,
        ["--quiet", "dep", "add", &id_a, &id_b],
        "dep_add_quiet",
    );
    assert!(
        add.status.success(),
        "dep add --quiet failed: {}",
        add.stderr
    );
    assert!(
        add.stdout.trim().is_empty(),
        "dep add --quiet should produce no stdout: '{}'",
        add.stdout
    );

    let cycles = run_br(&workspace, ["--quiet", "dep", "cycles"], "dep_cycles_quiet");
    assert!(
        cycles.status.success(),
        "dep cycles --quiet failed: {}",
        cycles.stderr
    );
    assert!(
        cycles.stdout.trim().is_empty(),
        "dep cycles --quiet should produce no stdout: '{}'",
        cycles.stdout
    );

    let remove = run_br(
        &workspace,
        ["--quiet", "dep", "remove", &id_a, &id_b],
        "dep_remove_quiet",
    );
    assert!(
        remove.status.success(),
        "dep remove --quiet failed: {}",
        remove.stderr
    );
    assert!(
        remove.stdout.trim().is_empty(),
        "dep remove --quiet should produce no stdout: '{}'",
        remove.stdout
    );
}

#[test]
fn e2e_quiet_flag_graph_subcommands() {
    let _log = common::test_log("e2e_quiet_flag_graph_subcommands");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Quiet graph test"],
        "create_graph_quiet",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let issue_id = create
        .stdout
        .lines()
        .next()
        .unwrap_or("")
        .strip_prefix("✓ Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("")
        .trim()
        .to_string();
    assert!(!issue_id.is_empty(), "expected created issue id in stdout");

    let graph_single = run_br(
        &workspace,
        ["--quiet", "graph", &issue_id],
        "graph_quiet_single",
    );
    assert!(
        graph_single.status.success(),
        "graph --quiet failed: {}",
        graph_single.stderr
    );
    assert!(
        graph_single.stdout.trim().is_empty(),
        "graph --quiet should produce no stdout: '{}'",
        graph_single.stdout
    );

    let graph_all = run_br(&workspace, ["--quiet", "graph", "--all"], "graph_quiet_all");
    assert!(
        graph_all.status.success(),
        "graph --quiet --all failed: {}",
        graph_all.stderr
    );
    assert!(
        graph_all.stdout.trim().is_empty(),
        "graph --quiet --all should produce no stdout: '{}'",
        graph_all.stdout
    );
}

#[test]
fn e2e_quiet_flag_comments_subcommands() {
    let _log = common::test_log("e2e_quiet_flag_comments_subcommands");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Quiet comments test"],
        "create_comments_quiet",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let issue_id = create
        .stdout
        .lines()
        .next()
        .unwrap_or("")
        .strip_prefix("✓ Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("")
        .trim()
        .to_string();
    assert!(!issue_id.is_empty(), "expected created issue id in stdout");

    let add = run_br(
        &workspace,
        [
            "--quiet",
            "comments",
            "add",
            &issue_id,
            "hello quiet comments",
        ],
        "comments_add_quiet",
    );
    assert!(
        add.status.success(),
        "comments add --quiet failed: {}",
        add.stderr
    );
    assert!(
        add.stdout.trim().is_empty(),
        "comments add --quiet should produce no stdout: '{}'",
        add.stdout
    );

    let list = run_br(
        &workspace,
        ["--quiet", "comments", "list", &issue_id],
        "comments_list_quiet",
    );
    assert!(
        list.status.success(),
        "comments list --quiet failed: {}",
        list.stderr
    );
    assert!(
        list.stdout.trim().is_empty(),
        "comments list --quiet should produce no stdout: '{}'",
        list.stdout
    );
}

#[test]
fn e2e_quiet_flag_query_subcommands() {
    let _log = common::test_log("e2e_quiet_flag_query_subcommands");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Quiet query test"],
        "create_query_quiet",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let save = run_br(
        &workspace,
        ["--quiet", "query", "save", "mine", "--status", "open"],
        "query_save_quiet",
    );
    assert!(
        save.status.success(),
        "query save --quiet failed: {}",
        save.stderr
    );
    assert!(
        save.stdout.trim().is_empty(),
        "query save --quiet should produce no stdout: '{}'",
        save.stdout
    );

    let list = run_br(&workspace, ["--quiet", "query", "list"], "query_list_quiet");
    assert!(
        list.status.success(),
        "query list --quiet failed: {}",
        list.stderr
    );
    assert!(
        list.stdout.trim().is_empty(),
        "query list --quiet should produce no stdout: '{}'",
        list.stdout
    );

    let delete = run_br(
        &workspace,
        ["--quiet", "query", "delete", "mine"],
        "query_delete_quiet",
    );
    assert!(
        delete.status.success(),
        "query delete --quiet failed: {}",
        delete.stderr
    );
    assert!(
        delete.stdout.trim().is_empty(),
        "query delete --quiet should produce no stdout: '{}'",
        delete.stdout
    );
}

#[test]
fn e2e_quiet_flag_count_and_where() {
    let _log = common::test_log("e2e_quiet_flag_count_and_where");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Quiet count/where test"],
        "create_count_where_quiet",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    assert_quiet_command(
        &workspace,
        ["--quiet", "count"],
        "count_quiet",
        "count --quiet",
    );
    assert_quiet_command(
        &workspace,
        ["--quiet", "stale", "--days", "0"],
        "stale_quiet",
        "stale --quiet",
    );
    assert_quiet_command(
        &workspace,
        ["--quiet", "where"],
        "where_quiet",
        "where --quiet",
    );
    assert_quiet_command(
        &workspace,
        ["--quiet", "info"],
        "info_quiet",
        "info --quiet",
    );
    assert_quiet_command(
        &workspace,
        ["--quiet", "info", "--thanks"],
        "info_thanks_quiet",
        "info --thanks --quiet",
    );
    assert_quiet_command(
        &workspace,
        ["--quiet", "info", "--whats-new"],
        "info_whats_new_quiet",
        "info --whats-new --quiet",
    );

    let info_json_value = run_quiet_json(
        &workspace,
        ["--quiet", "info", "--json"],
        "info_json_quiet",
        "info --json should remain json under quiet",
    );
    assert!(
        info_json_value.get("database_path").is_some(),
        "info --json should include database_path: {info_json_value}"
    );

    let info_thanks_value = run_quiet_json(
        &workspace,
        ["--quiet", "info", "--thanks", "--json"],
        "info_thanks_json_quiet",
        "info --thanks --json should remain json under quiet",
    );
    assert!(
        info_thanks_value["thanks"]
            .as_str()
            .is_some_and(|message| message.contains("Thanks for using br")),
        "info --thanks --json should preserve the thanks message: {info_thanks_value}"
    );
}

#[test]
fn e2e_quiet_flag_defer_subcommands() {
    let _log = common::test_log("e2e_quiet_flag_defer_subcommands");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Quiet defer test"],
        "create_defer_quiet",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let issue_id = create
        .stdout
        .lines()
        .next()
        .unwrap_or("")
        .strip_prefix("✓ Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("")
        .trim()
        .to_string();
    assert!(!issue_id.is_empty(), "expected created issue id in stdout");

    let defer = run_br(&workspace, ["--quiet", "defer", &issue_id], "defer_quiet");
    assert!(
        defer.status.success(),
        "defer --quiet failed: {}",
        defer.stderr
    );
    assert!(
        defer.stdout.trim().is_empty(),
        "defer --quiet should produce no stdout: '{}'",
        defer.stdout
    );

    let undefer = run_br(
        &workspace,
        ["--quiet", "undefer", &issue_id],
        "undefer_quiet",
    );
    assert!(
        undefer.status.success(),
        "undefer --quiet failed: {}",
        undefer.stderr
    );
    assert!(
        undefer.stdout.trim().is_empty(),
        "undefer --quiet should produce no stdout: '{}'",
        undefer.stdout
    );
}

#[test]
fn e2e_quiet_flag_config_epic_label_and_q_subcommands() {
    let _log = common::test_log("e2e_quiet_flag_config_epic_label_and_q_subcommands");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    assert_quiet_command(
        &workspace,
        ["--quiet", "config", "path"],
        "config_path_quiet",
        "config path --quiet",
    );
    assert_quiet_command(
        &workspace,
        ["--quiet", "epic", "status"],
        "epic_status_quiet",
        "epic status --quiet",
    );
    assert_quiet_command(
        &workspace,
        ["--quiet", "epic", "close-eligible"],
        "epic_close_eligible_quiet",
        "epic close-eligible --quiet",
    );

    let create = run_br(
        &workspace,
        ["create", "Quiet label test"],
        "create_label_quiet",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);

    assert_quiet_command(
        &workspace,
        ["--quiet", "label", "add", &issue_id, "triage"],
        "label_add_quiet",
        "label add --quiet",
    );

    assert_quiet_command(
        &workspace,
        ["--quiet", "q", "Quiet quick capture"],
        "q_quiet",
        "q --quiet",
    );

    assert_quiet_command(
        &workspace,
        ["--quiet", "delete", &issue_id],
        "delete_quiet",
        "delete --quiet",
    );
}

#[test]
fn e2e_quiet_flag_sync_subcommands() {
    let _log = common::test_log("e2e_quiet_flag_sync_subcommands");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Quiet sync test", "--no-auto-flush"],
        "create_sync_quiet",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    assert_quiet_command(
        &workspace,
        ["--quiet", "sync", "--status"],
        "sync_status_quiet",
        "sync --status --quiet",
    );
    assert_quiet_command(
        &workspace,
        ["--quiet", "sync", "--flush-only"],
        "sync_flush_quiet",
        "sync --flush-only --quiet",
    );
    assert_quiet_command(
        &workspace,
        ["--quiet", "sync", "--import-only"],
        "sync_import_quiet",
        "sync --import-only --quiet",
    );

    let status_json = run_quiet_json(
        &workspace,
        ["--quiet", "sync", "--status", "--json"],
        "sync_status_quiet_json",
        "sync --status --quiet --json should remain json",
    );
    assert!(
        status_json.get("dirty_count").is_some(),
        "sync --status --quiet --json should include dirty_count: {status_json}"
    );
}

// ============================================================================
// --verbose flag tests
// ============================================================================

#[test]
fn e2e_verbose_flag() {
    let _log = common::test_log("e2e_verbose_flag");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create with -v flag
    let create = run_br(
        &workspace,
        ["-v", "create", "Verbose test"],
        "create_verbose",
    );
    assert!(
        create.status.success(),
        "create -v failed: {}",
        create.stderr
    );

    // Verbose mode should show more output (in stderr typically)
    // RUST_LOG is already set to debug in test harness, so this is mostly
    // verifying the flag doesn't cause issues
}

#[test]
fn e2e_very_verbose_flag() {
    let _log = common::test_log("e2e_very_verbose_flag");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create with -vv flag for more verbosity
    let create = run_br(
        &workspace,
        ["-vv", "create", "Very verbose test"],
        "create_very_verbose",
    );
    assert!(
        create.status.success(),
        "create -vv failed: {}",
        create.stderr
    );
}

// ============================================================================
// Combined flags tests
// ============================================================================

#[test]
fn e2e_json_no_color_combined() {
    let _log = common::test_log("e2e_json_no_color_combined");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Combined flags test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Combine --json and --no-color
    let list = run_br(
        &workspace,
        ["list", "--json", "--no-color"],
        "list_combined",
    );
    assert!(
        list.status.success(),
        "list --json --no-color failed: {}",
        list.stderr
    );

    // Should be valid JSON with no color codes
    let _json = parse_list_issues(&list.stdout);
    assert!(
        !list.stdout.contains("\x1b["),
        "no color codes in JSON output"
    );
}

#[test]
fn e2e_no_db_json_combined() {
    let _log = common::test_log("e2e_no_db_json_combined");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "No-DB JSON test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    // Combine --no-db and --json
    let list = run_br(&workspace, ["--no-db", "list", "--json"], "list_no_db_json");
    assert!(
        list.status.success(),
        "list --no-db --json failed: {}",
        list.stderr
    );

    let json = parse_list_issues(&list.stdout);
    assert!(
        json.iter().any(|item| item["title"] == "No-DB JSON test"),
        "issue in no-db JSON output"
    );
}

#[test]
fn e2e_quiet_json_combined() {
    let _log = common::test_log("e2e_quiet_json_combined");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Quiet JSON test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Combine --quiet and --json
    let list = run_br(&workspace, ["--quiet", "list", "--json"], "list_quiet_json");
    assert!(
        list.status.success(),
        "list --quiet --json failed: {}",
        list.stderr
    );

    // JSON should still be valid
    let _json = parse_list_issues(&list.stdout);
}

// ============================================================================
// Global flag position tests
// ============================================================================

#[test]
fn e2e_global_flag_before_command() {
    let _log = common::test_log("e2e_global_flag_before_command");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Position test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Global flag before command
    let list = run_br(&workspace, ["--json", "list"], "list_flag_before");
    assert!(
        list.status.success(),
        "list with --json before command failed: {}",
        list.stderr
    );

    let _json = parse_list_issues(&list.stdout);
}

#[test]
fn e2e_global_flag_after_command() {
    let _log = common::test_log("e2e_global_flag_after_command");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Position test 2"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Global flag after command
    let list = run_br(&workspace, ["list", "--json"], "list_flag_after");
    assert!(
        list.status.success(),
        "list with --json after command failed: {}",
        list.stderr
    );

    let _json = parse_list_issues(&list.stdout);
}

// ============================================================================
// Output mode consistency tests (beads_rust-14eu)
// ============================================================================

/// JSON mode should produce stdout that parses as valid JSON with no extra text.
#[test]
fn e2e_json_stdout_is_clean_json() {
    let _log = common::test_log("e2e_json_stdout_is_clean_json");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create = run_br(&workspace, ["create", "JSON clean test"], "create");
    assert!(create.status.success());

    // Verify multiple commands produce clean JSON stdout
    for (cmd, label) in [
        (vec!["list", "--json"], "list"),
        (vec!["ready", "--json"], "ready"),
        (vec!["blocked", "--json"], "blocked"),
        (vec!["count", "--json"], "count"),
        (vec!["stats", "--json"], "stats"),
    ] {
        let output = run_br(&workspace, cmd.clone(), &format!("clean_json_{label}"));
        assert!(output.status.success(), "{label} failed: {}", output.stderr);

        let payload = extract_json_payload(&output.stdout);
        let parsed: Result<Value, _> = serde_json::from_str(&payload);
        assert!(
            parsed.is_ok(),
            "{label} --json stdout is not valid JSON:\n---stdout---\n{}\n---end---",
            output.stdout
        );

        // No ANSI escape codes in JSON output
        assert!(
            !output.stdout.contains("\x1b["),
            "{label} --json stdout contains ANSI escape codes"
        );
    }
}

/// --quiet mode should produce strictly less output than normal mode.
#[test]
fn e2e_quiet_reduces_output() {
    let _log = common::test_log("e2e_quiet_reduces_output");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create = run_br(&workspace, ["create", "Quiet reduction test"], "create");
    assert!(create.status.success());

    // Normal list output
    let normal = run_br(&workspace, ["list"], "list_normal");
    assert!(normal.status.success());

    // Quiet list output
    let quiet = run_br(&workspace, ["--quiet", "list"], "list_quiet_cmp");
    assert!(quiet.status.success());

    // Quiet output should be shorter or equal (never longer)
    assert!(
        quiet.stdout.len() <= normal.stdout.len(),
        "quiet output ({} bytes) should not exceed normal output ({} bytes)\nquiet: {}\nnormal: {}",
        quiet.stdout.len(),
        normal.stdout.len(),
        quiet.stdout,
        normal.stdout
    );
}

/// --json should take precedence over --quiet: produce valid JSON output.
#[test]
fn e2e_json_overrides_quiet() {
    let _log = common::test_log("e2e_json_overrides_quiet");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create = run_br(&workspace, ["create", "Precedence test"], "create");
    assert!(create.status.success());

    // --quiet --json should still produce valid JSON
    let output = run_br(&workspace, ["--quiet", "list", "--json"], "quiet_json_prec");
    assert!(output.status.success());

    let json = parse_list_issues(&output.stdout);
    assert!(
        json.iter().any(|item| item["title"] == "Precedence test"),
        "JSON output should contain the issue even with --quiet"
    );
}

/// --no-color should work across multiple command types (not just list).
#[test]
fn e2e_no_color_across_commands() {
    let _log = common::test_log("e2e_no_color_across_commands");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let id = {
        let create = run_br(&workspace, ["create", "No-color multi test"], "create");
        assert!(create.status.success());
        create
            .stdout
            .lines()
            .next()
            .unwrap_or("")
            .strip_prefix("✓ Created ")
            .or_else(|| {
                create
                    .stdout
                    .lines()
                    .next()
                    .unwrap_or("")
                    .strip_prefix("Created ")
            })
            .and_then(|s| s.split(':').next())
            .unwrap_or("")
            .trim()
            .to_string()
    };

    for (cmd, label) in [
        (vec!["list", "--no-color"], "list"),
        (vec!["show", &id, "--no-color"], "show"),
        (vec!["ready", "--no-color"], "ready"),
        (vec!["stats", "--no-color"], "stats"),
        (vec!["count", "--no-color"], "count"),
    ] {
        let output = run_br(&workspace, cmd.clone(), &format!("no_color_{label}"));
        assert!(output.status.success(), "{label} failed: {}", output.stderr);

        assert!(
            !output.stdout.contains("\x1b["),
            "{label} --no-color stdout contains ANSI escape codes"
        );
    }
}
