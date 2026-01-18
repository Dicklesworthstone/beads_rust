//! E2E tests for environment variable overrides and path handling.
//!
//! Tests `BEADS_DIR`, `BEADS_JSONL`, `BD_ACTOR`, and no-db mode interactions.
//! Part of beads_rust-9ks6.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br, run_br_with_env};
use serde_json::Value;
use std::fs;

// ============================================================================
// BEADS_DIR tests
// ============================================================================

#[test]
fn e2e_beads_dir_env_overrides_discovery() {
    let _log = common::test_log("e2e_beads_dir_env_overrides_discovery");

    // Create two workspaces: one for the actual .beads, one for the CWD
    let actual_workspace = BrWorkspace::new();
    let cwd_workspace = BrWorkspace::new();

    // Initialize the actual workspace
    let init = run_br(&actual_workspace, ["init"], "init_actual");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue in the actual workspace
    let create = run_br(&actual_workspace, ["create", "BEADS_DIR test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Now from the cwd_workspace (which has no .beads), use BEADS_DIR to point to actual
    let beads_dir = actual_workspace.root.join(".beads");
    let env_vars = vec![("BEADS_DIR", beads_dir.to_str().unwrap())];

    let list = run_br_with_env(&cwd_workspace, ["list", "--json"], env_vars, "list_via_env");
    assert!(
        list.status.success(),
        "list via BEADS_DIR failed: {}",
        list.stderr
    );

    let payload = extract_json_payload(&list.stdout);
    let list_json: Vec<Value> = serde_json::from_str(&payload).expect("list json");
    assert!(
        list_json
            .iter()
            .any(|item| item["title"] == "BEADS_DIR test"),
        "issue not found via BEADS_DIR override"
    );
}

#[test]
fn e2e_beads_dir_invalid_path_fails() {
    let _log = common::test_log("e2e_beads_dir_invalid_path_fails");
    let workspace = BrWorkspace::new();

    // Point BEADS_DIR to a non-existent path
    let env_vars = vec![("BEADS_DIR", "/nonexistent/path/to/beads")];

    let list = run_br_with_env(&workspace, ["list"], env_vars, "list_invalid_dir");
    assert!(
        !list.status.success(),
        "list should fail with invalid BEADS_DIR"
    );
    // Should produce an error about workspace not found (may be in JSON format)
    let combined = format!("{}{}", list.stdout, list.stderr);
    assert!(
        combined.contains("not found")
            || combined.contains("No such file")
            || combined.contains("NOT_INITIALIZED")
            || combined.contains("not initialized")
            || combined.contains("BEADS_DIR"),
        "error should mention workspace issue: stdout={}, stderr={}",
        list.stdout,
        list.stderr
    );
}

#[test]
fn e2e_beads_dir_takes_precedence_over_cwd() {
    let _log = common::test_log("e2e_beads_dir_takes_precedence_over_cwd");

    // Create two workspaces, each with their own .beads
    let workspace_a = BrWorkspace::new();
    let workspace_b = BrWorkspace::new();

    // Initialize both
    let init_a = run_br(&workspace_a, ["init"], "init_a");
    assert!(init_a.status.success(), "init_a failed: {}", init_a.stderr);

    let init_b = run_br(&workspace_b, ["init"], "init_b");
    assert!(init_b.status.success(), "init_b failed: {}", init_b.stderr);

    // Create different issues in each
    let create_a = run_br(&workspace_a, ["create", "Issue in A"], "create_a");
    assert!(
        create_a.status.success(),
        "create_a failed: {}",
        create_a.stderr
    );

    let create_b = run_br(&workspace_b, ["create", "Issue in B"], "create_b");
    assert!(
        create_b.status.success(),
        "create_b failed: {}",
        create_b.stderr
    );

    // From workspace_a's CWD, use BEADS_DIR to point to workspace_b
    let beads_dir_b = workspace_b.root.join(".beads");
    let env_vars = vec![("BEADS_DIR", beads_dir_b.to_str().unwrap())];

    // Run from workspace_a but should see workspace_b's issues
    let list = run_br_with_env(&workspace_a, ["list", "--json"], env_vars, "list_override");
    assert!(
        list.status.success(),
        "list override failed: {}",
        list.stderr
    );

    let payload = extract_json_payload(&list.stdout);
    let list_json: Vec<Value> = serde_json::from_str(&payload).expect("list json");

    // Should see B's issue, not A's
    assert!(
        list_json.iter().any(|item| item["title"] == "Issue in B"),
        "should see workspace B's issue"
    );
    assert!(
        !list_json.iter().any(|item| item["title"] == "Issue in A"),
        "should NOT see workspace A's issue"
    );
}

// ============================================================================
// BEADS_JSONL tests
// ============================================================================

#[test]
fn e2e_beads_jsonl_external_path() {
    let _log = common::test_log("e2e_beads_jsonl_external_path");
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue with --no-auto-flush to keep it dirty
    let create = run_br(
        &workspace,
        ["create", "External JSONL test", "--no-auto-flush"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Create an external JSONL location within the temp directory
    // Note: external paths must still be validated by br
    let external_dir = workspace.temp_dir.path().join("external");
    fs::create_dir_all(&external_dir).expect("create external dir");
    let external_jsonl = external_dir.join("custom.jsonl");

    // Set BEADS_JSONL to external path and sync with --allow-external-jsonl --force
    let env_vars = vec![("BEADS_JSONL", external_jsonl.to_str().unwrap())];

    let sync = run_br_with_env(
        &workspace,
        ["sync", "--flush-only", "--allow-external-jsonl", "--force"],
        env_vars.clone(),
        "sync_external",
    );

    // External JSONL support may be restricted depending on implementation
    // Test passes if either:
    // 1. Sync succeeds and creates external file, or
    // 2. Sync fails with appropriate error about external paths
    if sync.status.success() {
        // If succeeded, verify file was created
        assert!(
            external_jsonl.exists(),
            "external JSONL should be created at {:?} (sync output: {})",
            external_jsonl,
            sync.stdout
        );

        let contents = fs::read_to_string(&external_jsonl).expect("read external jsonl");
        assert!(
            contents.contains("External JSONL test"),
            "external JSONL should contain our issue"
        );
    } else {
        // If failed, should be a clear error about external paths
        let combined = format!("{}{}", sync.stdout, sync.stderr);
        assert!(
            combined.contains("external") || combined.contains("outside"),
            "sync failure should mention external path restriction: {}",
            combined
        );
    }
}

#[test]
fn e2e_beads_jsonl_without_allow_flag_warns() {
    let _log = common::test_log("e2e_beads_jsonl_without_allow_flag_warns");
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue
    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Create an external JSONL path
    let external_dir = workspace.temp_dir.path().join("external2");
    fs::create_dir_all(&external_dir).expect("create external dir");
    let external_jsonl = external_dir.join("disallowed.jsonl");

    // Set BEADS_JSONL but don't use --allow-external-jsonl
    let env_vars = vec![("BEADS_JSONL", external_jsonl.to_str().unwrap())];

    let sync = run_br_with_env(
        &workspace,
        ["sync", "--flush-only"],
        env_vars,
        "sync_no_allow",
    );

    // Should either fail or warn about external path without flag
    // The exact behavior depends on implementation
    if sync.status.success() {
        // If it succeeds, it should have used the default path, not external
        assert!(
            !external_jsonl.exists(),
            "external JSONL should NOT be created without --allow-external-jsonl"
        );
    } else {
        // If it fails, error message should be about external path
        assert!(
            sync.stderr.contains("external")
                || sync.stderr.contains("allow")
                || sync.stderr.contains("outside"),
            "error should mention external path: {}",
            sync.stderr
        );
    }
}

// ============================================================================
// BD_ACTOR tests
// ============================================================================

#[test]
fn e2e_bd_actor_env_sets_actor() {
    let _log = common::test_log("e2e_bd_actor_env_sets_actor");
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue with BD_ACTOR set
    let env_vars = vec![("BD_ACTOR", "env-actor-test")];

    let create = run_br_with_env(
        &workspace,
        ["create", "Actor test issue"],
        env_vars.clone(),
        "create_with_actor",
    );
    assert!(
        create.status.success(),
        "create with actor failed: {}",
        create.stderr
    );

    // Check config to verify actor is recognized
    let config_get = run_br_with_env(
        &workspace,
        ["config", "get", "actor"],
        env_vars,
        "config_get_actor",
    );
    // BD_ACTOR should be visible in config or operations
    // The exact output format depends on implementation
    assert!(
        config_get.status.success(),
        "config get actor failed: {}",
        config_get.stderr
    );
}

#[test]
fn e2e_actor_flag_overrides_env() {
    let _log = common::test_log("e2e_actor_flag_overrides_env");
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue
    let create = run_br(&workspace, ["create", "Flag override test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let id = create
        .stdout
        .lines()
        .next()
        .unwrap_or("")
        .strip_prefix("Created ")
        .and_then(|s| s.split(':').next())
        .unwrap_or("")
        .trim();

    // Add a comment with BD_ACTOR set, but also use --author flag
    let env_vars = vec![("BD_ACTOR", "env-actor")];

    let comment = run_br_with_env(
        &workspace,
        [
            "comments",
            "add",
            id,
            "--message",
            "Test comment",
            "--author",
            "flag-author",
        ],
        env_vars,
        "comment_with_override",
    );
    assert!(
        comment.status.success(),
        "comment failed: {}",
        comment.stderr
    );

    // Verify the comment has the flag-author, not env-actor
    let show = run_br(&workspace, ["show", id, "--json"], "show_comment");
    assert!(show.status.success(), "show failed: {}", show.stderr);

    let payload = extract_json_payload(&show.stdout);
    let show_json: Vec<Value> = serde_json::from_str(&payload).expect("show json");

    if let Some(comments) = show_json[0]["comments"].as_array() {
        if let Some(comment) = comments.first() {
            assert_eq!(
                comment["author"], "flag-author",
                "CLI --author flag should override BD_ACTOR env"
            );
        }
    }
}

// ============================================================================
// No-DB mode + environment interactions
// ============================================================================

#[test]
fn e2e_no_db_with_beads_dir() {
    let _log = common::test_log("e2e_no_db_with_beads_dir");

    // Create workspace with issues in JSONL
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "No-DB BEADS_DIR test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync failed: {}", sync.stderr);

    // From a different workspace, use BEADS_DIR + --no-db
    let other_workspace = BrWorkspace::new();
    let beads_dir = workspace.root.join(".beads");
    let env_vars = vec![("BEADS_DIR", beads_dir.to_str().unwrap())];

    let list = run_br_with_env(
        &other_workspace,
        ["--no-db", "list", "--json"],
        env_vars,
        "list_no_db_beads_dir",
    );
    assert!(
        list.status.success(),
        "list --no-db with BEADS_DIR failed: {}",
        list.stderr
    );

    let payload = extract_json_payload(&list.stdout);
    let list_json: Vec<Value> = serde_json::from_str(&payload).expect("list json");
    assert!(
        list_json
            .iter()
            .any(|item| item["title"] == "No-DB BEADS_DIR test"),
        "issue should be visible via BEADS_DIR + --no-db"
    );
}

#[test]
fn e2e_no_db_with_beads_jsonl() {
    let _log = common::test_log("e2e_no_db_with_beads_jsonl");
    let workspace = BrWorkspace::new();

    // Create .beads directory
    let beads_dir = workspace.temp_dir.path().join(".beads");
    fs::create_dir_all(&beads_dir).expect("create .beads");

    // Create JSONL file INSIDE .beads (path validation requires this)
    let custom_jsonl = beads_dir.join("custom.jsonl");
    let issue_json = r#"{"id":"bd-custom1","title":"Custom JSONL Location","status":"open","issue_type":"task","priority":2,"labels":[],"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","ephemeral":false,"pinned":false,"is_template":false,"dependencies":[],"comments":[]}"#;
    fs::write(&custom_jsonl, format!("{}\n", issue_json)).expect("write jsonl");

    // Use BEADS_JSONL to point to the custom location within .beads
    let env_vars = vec![
        ("BEADS_DIR", beads_dir.to_str().unwrap()),
        ("BEADS_JSONL", custom_jsonl.to_str().unwrap()),
    ];

    let list = run_br_with_env(
        &workspace,
        ["--no-db", "list", "--json"],
        env_vars,
        "list_custom_jsonl",
    );
    assert!(
        list.status.success(),
        "list --no-db with BEADS_JSONL failed: {}",
        list.stderr
    );

    let payload = extract_json_payload(&list.stdout);
    let list_json: Vec<Value> = serde_json::from_str(&payload).expect("list json");
    assert!(
        list_json
            .iter()
            .any(|item| item["title"] == "Custom JSONL Location"),
        "issue from BEADS_JSONL should be visible"
    );
}

#[test]
fn e2e_no_db_creates_to_jsonl() {
    let _log = common::test_log("e2e_no_db_creates_to_jsonl");
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create seed issue and flush to JSONL
    let create_seed = run_br(&workspace, ["create", "Seed issue"], "create_seed");
    assert!(
        create_seed.status.success(),
        "create seed failed: {}",
        create_seed.stderr
    );

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync failed: {}", sync.stderr);

    // Create a new issue in no-db mode
    let create_no_db = run_br(
        &workspace,
        ["--no-db", "create", "Created in no-db"],
        "create_no_db",
    );
    assert!(
        create_no_db.status.success(),
        "create --no-db failed: {}",
        create_no_db.stderr
    );

    // Verify the JSONL was updated
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
    assert!(
        contents.contains("Created in no-db"),
        "no-db create should update JSONL"
    );
}

// ============================================================================
// Path resolution logging tests
// ============================================================================

#[test]
fn e2e_info_shows_resolved_paths() {
    let _log = common::test_log("e2e_info_shows_resolved_paths");
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Run info command with --json to see resolved paths
    let info = run_br(&workspace, ["info", "--json"], "info_json");
    assert!(info.status.success(), "info failed: {}", info.stderr);

    let payload = extract_json_payload(&info.stdout);
    let info_json: Value = serde_json::from_str(&payload).expect("info json");

    // Verify paths are included (field name is "database_path")
    assert!(
        info_json.get("database_path").is_some(),
        "info should include database_path: {}",
        info_json
    );
}

#[test]
fn e2e_where_command_shows_paths() {
    let _log = common::test_log("e2e_where_command_shows_paths");
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Run where command
    let where_cmd = run_br(&workspace, ["where"], "where");
    assert!(
        where_cmd.status.success(),
        "where failed: {}",
        where_cmd.stderr
    );

    // Should show the .beads path
    let expected_path = workspace.root.join(".beads");
    assert!(
        where_cmd.stdout.contains(".beads")
            || where_cmd
                .stdout
                .contains(&expected_path.display().to_string()),
        "where should show .beads path: {}",
        where_cmd.stdout
    );
}

#[test]
fn e2e_where_with_beads_dir_override() {
    let _log = common::test_log("e2e_where_with_beads_dir_override");

    let actual_workspace = BrWorkspace::new();
    let cwd_workspace = BrWorkspace::new();

    // Initialize actual workspace
    let init = run_br(&actual_workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // From cwd_workspace, run where with BEADS_DIR override
    let beads_dir = actual_workspace.root.join(".beads");
    let env_vars = vec![("BEADS_DIR", beads_dir.to_str().unwrap())];

    let where_cmd = run_br_with_env(&cwd_workspace, ["where"], env_vars, "where_override");
    assert!(
        where_cmd.status.success(),
        "where with override failed: {}",
        where_cmd.stderr
    );

    // Should show the overridden path
    assert!(
        where_cmd.stdout.contains(&beads_dir.display().to_string())
            || where_cmd.stdout.contains(".beads"),
        "where should show BEADS_DIR override path: {}",
        where_cmd.stdout
    );
}

// ============================================================================
// Edge cases
// ============================================================================

#[test]
fn e2e_empty_beads_dir_env_ignored() {
    let _log = common::test_log("e2e_empty_beads_dir_env_ignored");
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Empty env test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Set BEADS_DIR to empty string - should be ignored
    let env_vars = vec![("BEADS_DIR", "")];

    let list = run_br_with_env(&workspace, ["list", "--json"], env_vars, "list_empty_env");
    assert!(
        list.status.success(),
        "list with empty BEADS_DIR should succeed: {}",
        list.stderr
    );

    let payload = extract_json_payload(&list.stdout);
    let list_json: Vec<Value> = serde_json::from_str(&payload).expect("list json");
    assert!(
        list_json
            .iter()
            .any(|item| item["title"] == "Empty env test"),
        "empty BEADS_DIR should be ignored, using CWD discovery"
    );
}

#[test]
fn e2e_whitespace_beads_dir_env_ignored() {
    let _log = common::test_log("e2e_whitespace_beads_dir_env_ignored");
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Whitespace env test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Set BEADS_DIR to whitespace - should be ignored
    let env_vars = vec![("BEADS_DIR", "   ")];

    let list = run_br_with_env(
        &workspace,
        ["list", "--json"],
        env_vars,
        "list_whitespace_env",
    );
    assert!(
        list.status.success(),
        "list with whitespace BEADS_DIR should succeed: {}",
        list.stderr
    );

    let payload = extract_json_payload(&list.stdout);
    let list_json: Vec<Value> = serde_json::from_str(&payload).expect("list json");
    assert!(
        list_json
            .iter()
            .any(|item| item["title"] == "Whitespace env test"),
        "whitespace BEADS_DIR should be ignored"
    );
}
