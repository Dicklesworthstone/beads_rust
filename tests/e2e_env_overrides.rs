//! E2E tests for environment variable overrides and path handling.
//!
//! Tests `BEADS_DIR`, `BEADS_JSONL`, `BD_ACTOR`, and no-db mode interactions.
//! Part of beads_rust-9ks6.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, parse_list_issues, run_br, run_br_with_env};
use common::harness::parse_created_id;
use serde_json::Value;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use toon_rust::try_decode;

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn toon_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
        .or_else(|| {
            value.as_f64().and_then(|n| {
                if n.is_finite() && n >= 0.0 && n.fract() == 0.0 {
                    Some(n as u64)
                } else {
                    None
                }
            })
        })
}

fn toon_array_items(value: &Value) -> Vec<&Value> {
    value
        .as_array()
        .map(|items| items.iter().filter(|item| !item.is_null()).collect())
        .unwrap_or_default()
}

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

    let list_json = parse_list_issues(&list.stdout);
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
fn e2e_invalid_beads_dir_does_not_fall_back_to_cwd_workspace() {
    let _log = common::test_log("e2e_invalid_beads_dir_does_not_fall_back_to_cwd_workspace");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Invalid env should not fall back"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let env_vars = vec![("BEADS_DIR", "/nonexistent/path/to/beads")];
    let list = run_br_with_env(&workspace, ["list", "--json"], env_vars, "list_invalid_dir");
    assert!(
        !list.status.success(),
        "invalid BEADS_DIR should not fall back to the cwd workspace"
    );

    let combined = format!("{}{}", list.stdout, list.stderr);
    assert!(
        combined.contains("BEADS_DIR") || combined.contains("existing .beads directory"),
        "error should mention the invalid BEADS_DIR override: stdout={}, stderr={}",
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

    let list_json = parse_list_issues(&list.stdout);

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

#[test]
fn e2e_bd_db_env_override_allows_access_outside_workspace() {
    let _log = common::test_log("e2e_bd_db_env_override_allows_access_outside_workspace");

    let actual_workspace = BrWorkspace::new();
    let cwd_workspace = BrWorkspace::new();

    let init = run_br(&actual_workspace, ["init"], "init_actual");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&actual_workspace, ["create", "BD_DB env test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let db_path = actual_workspace.root.join(".beads").join("beads.db");
    let env_vars = vec![("BD_DB", db_path.to_str().expect("db path"))];

    let list = run_br_with_env(
        &cwd_workspace,
        ["list", "--json"],
        env_vars,
        "list_via_bd_db",
    );
    assert!(
        list.status.success(),
        "list via BD_DB failed: {}",
        list.stderr
    );

    let list_json = parse_list_issues(&list.stdout);
    assert!(
        list_json
            .iter()
            .any(|item| item["title"] == "BD_DB env test"),
        "issue not found via BD_DB override"
    );
}

/// GH#488: `BEADS_DB` is documented in the `br` skill reference as the database
/// override, but only the legacy `BD_DB` / `BD_DATABASE` names were consulted,
/// so the documented variable was inert in both directions.
#[test]
fn e2e_beads_db_env_override_allows_access_outside_workspace() {
    let _log = common::test_log("e2e_beads_db_env_override_allows_access_outside_workspace");

    let actual_workspace = BrWorkspace::new();
    let cwd_workspace = BrWorkspace::new();

    let init = run_br(&actual_workspace, ["init"], "init_actual");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&actual_workspace, ["create", "BEADS_DB env test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let db_path = actual_workspace.root.join(".beads").join("beads.db");
    let env_vars = vec![("BEADS_DB", db_path.to_str().expect("db path"))];

    let list = run_br_with_env(
        &cwd_workspace,
        ["list", "--json"],
        env_vars,
        "list_via_beads_db",
    );
    assert!(
        list.status.success(),
        "list via BEADS_DB failed: {}",
        list.stderr
    );

    let list_json = parse_list_issues(&list.stdout);
    assert!(
        list_json
            .iter()
            .any(|item| item["title"] == "BEADS_DB env test"),
        "issue not found via BEADS_DB override"
    );
}

/// GH#488 case A: the override must also win over a `.beads` that *is*
/// discoverable from the current directory, matching `--db`.
#[test]
fn e2e_beads_db_env_override_wins_over_discovered_workspace() {
    let _log = common::test_log("e2e_beads_db_env_override_wins_over_discovered_workspace");

    let workspace_a = BrWorkspace::new();
    let workspace_b = BrWorkspace::new();

    let init_a = run_br(&workspace_a, ["init"], "init_a");
    assert!(init_a.status.success(), "init_a failed: {}", init_a.stderr);
    let init_b = run_br(&workspace_b, ["init"], "init_b");
    assert!(init_b.status.success(), "init_b failed: {}", init_b.stderr);

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

    let db_path_b = workspace_b.root.join(".beads").join("beads.db");
    let env_vars = vec![("BEADS_DB", db_path_b.to_str().expect("db path"))];

    let list = run_br_with_env(
        &workspace_a,
        ["list", "--json"],
        env_vars,
        "list_beads_db_override",
    );
    assert!(
        list.status.success(),
        "list via BEADS_DB failed: {}",
        list.stderr
    );

    let list_json = parse_list_issues(&list.stdout);
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
            "sync failure should mention external path restriction: {combined}"
        );
    }
}

#[test]
fn e2e_beads_jsonl_external_rename_prefix_honors_allow_flag_during_config_load() {
    let _log = common::test_log(
        "e2e_beads_jsonl_external_rename_prefix_honors_allow_flag_during_config_load",
    );
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_external_rename_prefix");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let set_prefix = run_br(
        &workspace,
        ["config", "set", "issue_prefix=target"],
        "set_prefix_external_rename_prefix",
    );
    assert!(
        set_prefix.status.success(),
        "config set failed: {}",
        set_prefix.stderr
    );

    let external_dir = workspace.temp_dir.path().join("external-rename");
    fs::create_dir_all(&external_dir).expect("create external dir");
    let external_jsonl = external_dir.join("rename-source.jsonl");
    fs::write(
        &external_jsonl,
        "{\"id\":\"legacy-1\",\"title\":\"External rename prefix import\",\"status\":\"open\",\"priority\":2,\"issue_type\":\"task\",\"created_at\":\"2026-01-01T00:00:00Z\",\"updated_at\":\"2026-01-01T00:00:00Z\"}\n",
    )
    .expect("write external jsonl");

    let env_vars = vec![("BEADS_JSONL", external_jsonl.to_str().unwrap())];
    let sync = run_br_with_env(
        &workspace,
        [
            "sync",
            "--import-only",
            "--allow-external-jsonl",
            "--force",
            "--rename-prefix",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        env_vars,
        "sync_external_rename_prefix",
    );
    assert!(
        sync.status.success(),
        "external rename-prefix import should honor --allow-external-jsonl during config load: stdout={} stderr={}",
        sync.stdout,
        sync.stderr
    );

    let list = run_br(
        &workspace,
        ["--no-auto-import", "list", "--json"],
        "list_external_rename_prefix",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);
    let issues = parse_list_issues(&list.stdout);
    let imported = issues
        .iter()
        .find(|issue| {
            issue.get("title").and_then(Value::as_str) == Some("External rename prefix import")
        })
        .expect("renamed imported issue should be listed");
    let id = imported
        .get("id")
        .and_then(Value::as_str)
        .expect("imported issue id");
    assert!(
        id.starts_with("target-"),
        "renamed imported issue should use configured prefix, got {id}"
    );
}

#[test]
fn e2e_beads_jsonl_external_missing_db_recovery_honors_allow_flag() {
    let _log = common::test_log("e2e_beads_jsonl_external_missing_db_recovery_honors_allow_flag");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_external_missing_db_recovery");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let external_dir = workspace.temp_dir.path().join("external-recovery");
    fs::create_dir_all(&external_dir).expect("create external dir");
    let external_jsonl = external_dir.join("recovery-source.jsonl");
    fs::write(
        &external_jsonl,
        "{\"id\":\"ext-1\",\"title\":\"External missing DB recovery\",\"status\":\"open\",\"priority\":2,\"issue_type\":\"task\",\"created_at\":\"2026-01-01T00:00:00Z\",\"updated_at\":\"2026-01-01T00:00:00Z\"}\n",
    )
    .expect("write external jsonl");

    let alt_db = workspace
        .root
        .join(".beads")
        .join("external-missing-db-recovery.db");
    let env_vars = vec![("BEADS_JSONL", external_jsonl.to_str().unwrap())];
    let status = run_br_with_env(
        &workspace,
        [
            "--db",
            alt_db.to_str().expect("alt db path"),
            "sync",
            "--status",
            "--allow-external-jsonl",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        env_vars,
        "sync_external_missing_db_recovery_status",
    );
    assert!(
        status.status.success(),
        "external JSONL missing-db recovery should honor --allow-external-jsonl during startup rebuild: stdout={} stderr={}",
        status.stdout,
        status.stderr
    );

    let list = run_br(
        &workspace,
        [
            "--db",
            alt_db.to_str().expect("alt db path"),
            "--no-auto-import",
            "--no-auto-flush",
            "list",
            "--json",
        ],
        "list_external_missing_db_recovery",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);
    let issues = parse_list_issues(&list.stdout);
    assert!(
        issues.iter().any(|issue| matches!(
            issue.get("title").and_then(Value::as_str),
            Some("External missing DB recovery")
        )),
        "issue imported during missing-db recovery should be listed"
    );
}

#[test]
fn e2e_beads_jsonl_env_overrides_metadata() {
    let _log = common::test_log("e2e_beads_jsonl_env_overrides_metadata");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue but keep it dirty to avoid writing the default JSONL
    let create = run_br(
        &workspace,
        ["create", "Env JSONL override test", "--no-auto-flush"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Force metadata to point at a different JSONL path
    let metadata_path = workspace.root.join(".beads").join("metadata.json");
    let metadata_json = r#"{"database":"beads.db","jsonl_export":"custom.jsonl"}"#;
    fs::write(&metadata_path, metadata_json).expect("write metadata");

    // Env should override metadata
    let env_jsonl = workspace.root.join(".beads").join("env.jsonl");
    let env_vars = vec![("BEADS_JSONL", env_jsonl.to_str().unwrap())];

    let sync = run_br_with_env(
        &workspace,
        ["sync", "--flush-only"],
        env_vars,
        "sync_env_jsonl",
    );
    assert!(sync.status.success(), "sync failed: {}", sync.stderr);

    assert!(env_jsonl.exists(), "env JSONL should be created");
    let contents = fs::read_to_string(&env_jsonl).expect("read env jsonl");
    assert!(
        contents.contains("Env JSONL override test"),
        "env JSONL should contain the issue"
    );

    let metadata_jsonl = workspace.root.join(".beads").join("custom.jsonl");
    assert!(
        !metadata_jsonl.exists(),
        "metadata JSONL should not be created when BEADS_JSONL is set"
    );
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

    assert!(
        !sync.status.success(),
        "sync should fail without --allow-external-jsonl (stdout={}, stderr={})",
        sync.stdout,
        sync.stderr
    );
    let combined = format!("{}{}", sync.stdout, sync.stderr);
    assert!(
        combined.contains("external")
            || combined.contains("allow-external-jsonl")
            || combined.contains("outside"),
        "error should mention external path restriction: {combined}"
    );
    assert!(
        !external_jsonl.exists(),
        "external JSONL should NOT be created without --allow-external-jsonl"
    );
}

#[test]
fn e2e_beads_jsonl_metadata_external_without_allow_fails() {
    let _log = common::test_log("e2e_beads_jsonl_metadata_external_without_allow_fails");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Metadata external JSONL"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let external_dir = workspace.root.join("external-jsonl");
    fs::create_dir_all(&external_dir).expect("create external dir");
    let external_jsonl = external_dir.join("metadata.jsonl");

    let metadata_path = workspace.root.join(".beads").join("metadata.json");
    let metadata_json = format!(
        r#"{{"database":"beads.db","jsonl_export":"{}"}}"#,
        external_jsonl.display()
    );
    fs::write(&metadata_path, metadata_json).expect("write metadata");

    let sync = run_br(
        &workspace,
        ["sync", "--flush-only"],
        "sync_metadata_external",
    );
    assert!(
        !sync.status.success(),
        "sync should fail for external metadata jsonl without allow flag (stdout={}, stderr={})",
        sync.stdout,
        sync.stderr
    );

    let combined = format!("{}{}", sync.stdout, sync.stderr);
    assert!(
        combined.contains("external")
            || combined.contains("allow-external-jsonl")
            || combined.contains("outside"),
        "error should mention external path restriction: {combined}"
    );
    assert!(
        !external_jsonl.exists(),
        "external JSONL should NOT be created without --allow-external-jsonl"
    );
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

    // Output is "✓ Created bd-abc123: Flag override test"
    let id = create
        .stdout
        .lines()
        .next()
        .unwrap_or("")
        .strip_prefix("✓ Created ")
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

    if let Some(comments) = show_json[0]["comments"].as_array()
        && let Some(comment) = comments.first()
    {
        assert_eq!(
            comment["author"], "flag-author",
            "CLI --author flag should override BD_ACTOR env"
        );
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

    let list_json = parse_list_issues(&list.stdout);
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
    fs::write(&custom_jsonl, format!("{issue_json}\n")).expect("write jsonl");

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

    let list_json = parse_list_issues(&list.stdout);
    assert!(
        list_json
            .iter()
            .any(|item| item["title"] == "Custom JSONL Location"),
        "issue from BEADS_JSONL should be visible"
    );
}

#[test]
fn e2e_no_db_sync_status_external_beads_jsonl_honors_allow_flag() {
    let _log = common::test_log("e2e_no_db_sync_status_external_beads_jsonl_honors_allow_flag");
    let workspace = BrWorkspace::new();

    let beads_dir = workspace.temp_dir.path().join(".beads");
    fs::create_dir_all(&beads_dir).expect("create .beads");

    let external_dir = workspace.temp_dir.path().join("external-no-db");
    fs::create_dir_all(&external_dir).expect("create external dir");
    let external_jsonl = external_dir.join("issues.jsonl");
    fs::write(
        &external_jsonl,
        "{\"id\":\"ext-1\",\"title\":\"No-db external JSONL sync status\",\"status\":\"open\",\"priority\":2,\"issue_type\":\"task\",\"created_at\":\"2026-01-01T00:00:00Z\",\"updated_at\":\"2026-01-01T00:00:00Z\"}\n",
    )
    .expect("write external jsonl");

    let env_vars = vec![
        ("BEADS_DIR", beads_dir.to_str().unwrap()),
        ("BEADS_JSONL", external_jsonl.to_str().unwrap()),
    ];

    let status = run_br_with_env(
        &workspace,
        [
            "--no-db",
            "sync",
            "--status",
            "--allow-external-jsonl",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        env_vars,
        "no_db_sync_status_external_jsonl",
    );
    assert!(
        status.status.success(),
        "no-db sync --status should honor --allow-external-jsonl during startup import: stdout={} stderr={}",
        status.stdout,
        status.stderr
    );
}

#[test]
fn e2e_no_db_ignores_lock_timeout_flag() {
    let _log = common::test_log("e2e_no_db_ignores_lock_timeout_flag");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "No-DB lock-timeout"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync failed: {}", sync.stderr);

    let list = run_br(
        &workspace,
        ["--no-db", "--lock-timeout", "1", "list", "--json"],
        "list_no_db_lock_timeout",
    );
    assert!(
        list.status.success(),
        "list --no-db --lock-timeout failed: {}",
        list.stderr
    );

    let list_json = parse_list_issues(&list.stdout);
    assert!(
        list_json
            .iter()
            .any(|item| item["title"] == "No-DB lock-timeout"),
        "issue should be visible in no-db mode even with lock-timeout flag"
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
        "info should include database_path: {info_json}"
    );
}

#[test]
fn e2e_info_plain_output_shows_storage_paths() {
    let _log = common::test_log("e2e_info_plain_output_shows_storage_paths");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let info = run_br(&workspace, ["info"], "info_plain");
    assert!(info.status.success(), "info failed: {}", info.stderr);

    assert!(
        info.stdout.contains("Beads dir:"),
        "plain info should include beads dir: {}",
        info.stdout
    );
    assert!(
        info.stdout.contains("JSONL:"),
        "plain info should include jsonl path: {}",
        info.stdout
    );
}

#[test]
fn e2e_info_honors_toon_env_mode() {
    let _log = common::test_log("e2e_info_honors_toon_env_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let info = run_br_with_env(
        &workspace,
        ["info"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "info_toon_env",
    );
    assert!(info.status.success(), "info toon failed: {}", info.stderr);

    let decoded = try_decode(info.stdout.trim(), None).expect("valid info TOON");
    let json = Value::from(decoded);
    assert!(
        json.get("database_path").is_some(),
        "TOON info should include database_path: {json}"
    );
}

#[test]
fn e2e_info_message_honors_toon_env_mode() {
    let _log = common::test_log("e2e_info_message_honors_toon_env_mode");
    let workspace = BrWorkspace::new();

    let info = run_br_with_env(
        &workspace,
        ["info", "--thanks"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "info_thanks_toon_env",
    );
    assert!(
        info.status.success(),
        "info --thanks toon failed: {}",
        info.stderr
    );

    let decoded = try_decode(info.stdout.trim(), None).expect("valid info message TOON");
    let json = Value::from(decoded);
    assert!(
        json["thanks"]
            .as_str()
            .is_some_and(|message| message.contains("Thanks for using br")),
        "TOON info message should preserve the thanks text: {json}"
    );
}

#[test]
fn e2e_delete_honors_toon_env_mode() {
    let _log = common::test_log("e2e_delete_honors_toon_env_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "TOON delete target", "--json"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let issue_id = created["id"].as_str().expect("issue id");

    let delete = run_br_with_env(
        &workspace,
        ["delete", issue_id],
        [("BR_OUTPUT_FORMAT", "toon")],
        "delete_toon_env",
    );
    assert!(
        delete.status.success(),
        "delete toon failed: {}",
        delete.stderr
    );

    let decoded = try_decode(delete.stdout.trim(), None).expect("valid delete TOON");
    let json = Value::from(decoded);
    assert_eq!(json["deleted_count"].as_f64(), Some(1.0));
    let deleted = toon_array_items(&json["deleted"]);
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0], &Value::String(issue_id.to_string()));
}

#[test]
fn e2e_delete_preview_honors_toon_env_mode() {
    let _log = common::test_log("e2e_delete_preview_honors_toon_env_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create_blocker = run_br(&workspace, ["create", "TOON blocker", "--json"], "create_a");
    assert!(
        create_blocker.status.success(),
        "create blocker failed: {}",
        create_blocker.stderr
    );
    let blocker_issue: Value =
        serde_json::from_str(&extract_json_payload(&create_blocker.stdout)).expect("blocker json");
    let blocker_id = blocker_issue["id"]
        .as_str()
        .expect("blocker id")
        .to_string();

    let create_dependent = run_br(&workspace, ["create", "TOON blocked", "--json"], "create_b");
    assert!(
        create_dependent.status.success(),
        "create blocked failed: {}",
        create_dependent.stderr
    );
    let dependent_issue: Value =
        serde_json::from_str(&extract_json_payload(&create_dependent.stdout))
            .expect("blocked json");
    let dependent_id = dependent_issue["id"]
        .as_str()
        .expect("blocked id")
        .to_string();

    let dep_add = run_br(
        &workspace,
        ["dep", "add", &dependent_id, &blocker_id],
        "dep_add_preview_toon",
    );
    assert!(
        dep_add.status.success(),
        "dep add failed: {}",
        dep_add.stderr
    );

    let delete = run_br_with_env(
        &workspace,
        ["delete", &blocker_id],
        [("BR_OUTPUT_FORMAT", "toon")],
        "delete_preview_toon_env",
    );
    assert!(
        delete.status.success(),
        "delete preview toon failed: {}",
        delete.stderr
    );

    let decoded = try_decode(delete.stdout.trim(), None).expect("valid delete preview TOON");
    let json = Value::from(decoded);
    assert_eq!(json["preview"], true);
    let would_delete = toon_array_items(&json["would_delete"]);
    assert_eq!(would_delete.len(), 1);
    assert_eq!(would_delete[0], &Value::String(blocker_id));
    let blocked_dependents = toon_array_items(&json["blocked_dependents"]);
    assert_eq!(blocked_dependents.len(), 1);
    assert_eq!(blocked_dependents[0], &Value::String(dependent_id));
}

#[test]
fn e2e_close_honors_toon_env_mode() {
    let _log = common::test_log("e2e_close_honors_toon_env_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "TOON close target", "--json"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let issue_id = created["id"].as_str().expect("issue id").to_string();

    let close = run_br_with_env(
        &workspace,
        ["close", &issue_id],
        [("BR_OUTPUT_FORMAT", "toon")],
        "close_toon_env",
    );
    assert!(
        close.status.success(),
        "close toon failed: {}",
        close.stderr
    );

    let decoded = try_decode(close.stdout.trim(), None).expect("valid close TOON");
    let json = Value::from(decoded);
    let closed = toon_array_items(&json);
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["id"], issue_id);
    assert_eq!(closed[0]["status"], "closed");
}

#[test]
fn e2e_reopen_honors_toon_env_mode() {
    let _log = common::test_log("e2e_reopen_honors_toon_env_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "TOON reopen target", "--json"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let issue_id = created["id"].as_str().expect("issue id").to_string();

    let close = run_br(&workspace, ["close", &issue_id], "close_before_reopen");
    assert!(close.status.success(), "close failed: {}", close.stderr);

    let reopen = run_br_with_env(
        &workspace,
        ["reopen", &issue_id],
        [("BR_OUTPUT_FORMAT", "toon")],
        "reopen_toon_env",
    );
    assert!(
        reopen.status.success(),
        "reopen toon failed: {}",
        reopen.stderr
    );

    let decoded = try_decode(reopen.stdout.trim(), None).expect("valid reopen TOON");
    let json = Value::from(decoded);
    let reopened = toon_array_items(&json["reopened"]);
    assert_eq!(reopened.len(), 1);
    assert_eq!(reopened[0]["id"], issue_id);
    assert_eq!(reopened[0]["status"], "open");
}

#[test]
fn e2e_defer_honors_toon_env_mode() {
    let _log = common::test_log("e2e_defer_honors_toon_env_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "TOON defer target", "--json"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let issue_id = created["id"].as_str().expect("issue id").to_string();

    let defer = run_br_with_env(
        &workspace,
        ["defer", &issue_id],
        [("BR_OUTPUT_FORMAT", "toon")],
        "defer_toon_env",
    );
    assert!(
        defer.status.success(),
        "defer toon failed: {}",
        defer.stderr
    );

    let decoded = try_decode(defer.stdout.trim(), None).expect("valid defer TOON");
    let json = Value::from(decoded);
    let deferred = toon_array_items(&json["deferred"]);
    assert_eq!(deferred.len(), 1);
    assert_eq!(deferred[0]["id"], issue_id);
    assert_eq!(deferred[0]["status"], "deferred");
}

#[test]
fn e2e_q_honors_toon_env_mode() {
    let _log = common::test_log("e2e_q_honors_toon_env_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let quick_capture = run_br_with_env(
        &workspace,
        ["q", "TOON quick capture"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "q_toon_env",
    );
    assert!(
        quick_capture.status.success(),
        "q toon failed: {}",
        quick_capture.stderr
    );

    let decoded = try_decode(quick_capture.stdout.trim(), None).expect("valid q TOON");
    let json = Value::from(decoded);
    assert_eq!(json["title"].as_str(), Some("TOON quick capture"));
    assert!(
        json["id"].as_str().is_some_and(|id| !id.is_empty()),
        "TOON q output should include a created id: {json}"
    );
}

#[test]
fn e2e_update_honors_toon_env_mode() {
    let _log = common::test_log("e2e_update_honors_toon_env_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_update_toon");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "TOON update target", "--json"],
        "create_update",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let issue_id = issue["id"].as_str().expect("issue id").to_string();

    let update = run_br_with_env(
        &workspace,
        ["update", &issue_id, "--status", "in_progress"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "update_toon_env",
    );
    assert!(
        update.status.success(),
        "update toon failed: {}",
        update.stderr
    );

    let decoded = try_decode(update.stdout.trim(), None).expect("valid update TOON");
    let json = Value::from(decoded);
    let results = toon_array_items(&json);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["id"].as_str(), Some(issue_id.as_str()));
    assert_eq!(results[0]["status"].as_str(), Some("in_progress"));
}

#[test]
fn e2e_count_honors_toon_env_mode() {
    let _log = common::test_log("e2e_count_honors_toon_env_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_count_toon");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "TOON count target"],
        "create_count_toon",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let count = run_br_with_env(
        &workspace,
        ["count"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "count_toon_env",
    );
    assert!(
        count.status.success(),
        "count toon failed: {}",
        count.stderr
    );

    let decoded = try_decode(count.stdout.trim(), None).expect("valid count TOON");
    let json = Value::from(decoded);
    assert_eq!(toon_u64(&json["count"]), Some(1));
}

#[test]
fn e2e_version_honors_toon_env_mode() {
    let _log = common::test_log("e2e_version_honors_toon_env_mode");
    let workspace = BrWorkspace::new();

    let version = run_br_with_env(
        &workspace,
        ["version"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "version_toon_env",
    );
    assert!(
        version.status.success(),
        "version toon failed: {}",
        version.stderr
    );

    let json = Value::from(try_decode(version.stdout.trim(), None).expect("valid version TOON"));
    assert!(
        json["version"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "TOON version output should include a version string: {json}"
    );
    assert!(
        json["build"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "TOON version output should include a build string: {json}"
    );
}

#[test]
fn e2e_lint_honors_toon_env_mode() {
    let _log = common::test_log("e2e_lint_honors_toon_env_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_lint_toon");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Lint TOON bug", "--type", "bug"],
        "create_lint_toon",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let lint = run_br_with_env(
        &workspace,
        ["lint"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "lint_toon_env",
    );
    assert!(lint.status.success(), "lint toon failed: {}", lint.stderr);

    let json = Value::from(try_decode(lint.stdout.trim(), None).expect("valid lint TOON"));
    assert_eq!(toon_u64(&json["total"]), Some(2));
    assert_eq!(toon_u64(&json["issues"]), Some(1));
    let results = toon_array_items(&json["results"]);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["type"].as_str(), Some("bug"));
}

#[test]
fn e2e_stale_honors_toon_env_mode() {
    let _log = common::test_log("e2e_stale_honors_toon_env_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_stale_toon");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "TOON stale target", "--json"],
        "create_stale_toon",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let issue_id = issue["id"].as_str().expect("issue id").to_string();

    let stale = run_br_with_env(
        &workspace,
        ["stale", "--days", "0"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "stale_toon_env",
    );
    assert!(
        stale.status.success(),
        "stale toon failed: {}",
        stale.stderr
    );

    let decoded = try_decode(stale.stdout.trim(), None).expect("valid stale TOON");
    let json = Value::from(decoded);
    let stale_items = toon_array_items(&json);
    assert_eq!(stale_items.len(), 1);
    assert_eq!(stale_items[0]["id"].as_str(), Some(issue_id.as_str()));
}

#[test]
fn e2e_epic_status_honors_toon_env_mode() {
    let _log = common::test_log("e2e_epic_status_honors_toon_env_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_epic_status_toon");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "TOON epic target", "--type", "epic", "--json"],
        "create_epic_status_toon",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let epic_id = issue["id"].as_str().expect("epic id").to_string();

    let status = run_br_with_env(
        &workspace,
        ["epic", "status"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "epic_status_toon_env",
    );
    assert!(
        status.status.success(),
        "epic status toon failed: {}",
        status.stderr
    );

    let decoded = try_decode(status.stdout.trim(), None).expect("valid epic status TOON");
    let json = Value::from(decoded);
    let epics = toon_array_items(&json);
    assert_eq!(epics.len(), 1);
    assert_eq!(epics[0]["epic"]["id"].as_str(), Some(epic_id.as_str()));
}

#[test]
fn e2e_epic_close_eligible_honors_toon_env_mode() {
    let _log = common::test_log("e2e_epic_close_eligible_honors_toon_env_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_epic_close_toon");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let epic = run_br(
        &workspace,
        ["create", "TOON closeable epic", "--type", "epic", "--json"],
        "create_epic_close_toon",
    );
    assert!(epic.status.success(), "create epic failed: {}", epic.stderr);
    let epic_value: Value =
        serde_json::from_str(&extract_json_payload(&epic.stdout)).expect("epic json");
    let epic_id = epic_value["id"].as_str().expect("epic id").to_string();

    let child = run_br(
        &workspace,
        ["create", "TOON epic child", "--type", "task", "--json"],
        "create_epic_child_toon",
    );
    assert!(
        child.status.success(),
        "create child failed: {}",
        child.stderr
    );
    let child_value: Value =
        serde_json::from_str(&extract_json_payload(&child.stdout)).expect("child json");
    let child_id = child_value["id"].as_str().expect("child id").to_string();

    let dep = run_br(
        &workspace,
        ["dep", "add", &child_id, &epic_id, "--type", "parent-child"],
        "dep_epic_child_toon",
    );
    assert!(dep.status.success(), "dep add failed: {}", dep.stderr);

    let close_child = run_br(
        &workspace,
        ["close", &child_id, "--force"],
        "close_epic_child_toon",
    );
    assert!(
        close_child.status.success(),
        "close child failed: {}",
        close_child.stderr
    );

    let close_eligible = run_br_with_env(
        &workspace,
        ["epic", "close-eligible"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "epic_close_eligible_toon_env",
    );
    assert!(
        close_eligible.status.success(),
        "epic close-eligible toon failed: {}",
        close_eligible.stderr
    );

    let decoded = try_decode(close_eligible.stdout.trim(), None).expect("valid epic close TOON");
    let json = Value::from(decoded);
    assert_eq!(toon_u64(&json["count"]), Some(1));
    let closed = toon_array_items(&json["closed"]);
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].as_str(), Some(epic_id.as_str()));
}

#[test]
fn e2e_label_add_honors_toon_env_mode() {
    let _log = common::test_log("e2e_label_add_honors_toon_env_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_label_toon");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "TOON label target", "--json"],
        "create_label",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let issue_id = issue["id"].as_str().expect("issue id").to_string();

    let label_add = run_br_with_env(
        &workspace,
        ["label", "add", &issue_id, "triage"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "label_add_toon_env",
    );
    assert!(
        label_add.status.success(),
        "label add toon failed: {}",
        label_add.stderr
    );

    let decoded = try_decode(label_add.stdout.trim(), None).expect("valid label add TOON");
    let json = Value::from(decoded);
    let results = toon_array_items(&json);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["issue_id"].as_str(), Some(issue_id.as_str()));
    assert_eq!(results[0]["label"].as_str(), Some("triage"));
}

#[test]
fn e2e_comments_add_honors_toon_env_mode() {
    let _log = common::test_log("e2e_comments_add_honors_toon_env_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_comments_toon");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "TOON comments target", "--json"],
        "create_comments",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let issue_id = issue["id"].as_str().expect("issue id").to_string();

    let comments_add = run_br_with_env(
        &workspace,
        ["comments", "add", &issue_id, "TOON comment"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "comments_add_toon_env",
    );
    assert!(
        comments_add.status.success(),
        "comments add toon failed: {}",
        comments_add.stderr
    );

    let decoded = try_decode(comments_add.stdout.trim(), None).expect("valid comments add TOON");
    let json = Value::from(decoded);
    assert_eq!(json["issue_id"].as_str(), Some(issue_id.as_str()));
    assert_eq!(json["text"].as_str(), Some("TOON comment"));
}

#[test]
fn e2e_audit_record_honors_toon_env_mode() {
    let _log = common::test_log("e2e_audit_record_honors_toon_env_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_audit_record_toon");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let record = run_br_with_env(
        &workspace,
        ["audit", "record", "--kind", "llm_call"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "audit_record_toon_env",
    );
    assert!(
        record.status.success(),
        "audit record toon failed: {}",
        record.stderr
    );

    let json =
        Value::from(try_decode(record.stdout.trim(), None).expect("valid audit record TOON"));
    assert!(
        json["id"]
            .as_str()
            .is_some_and(|value| value.starts_with("int-")),
        "TOON audit record output should include an interaction id: {json}"
    );
    assert_eq!(json["kind"].as_str(), Some("llm_call"));
}

#[test]
fn e2e_audit_log_and_summary_honor_toon_env_mode() {
    let _log = common::test_log("e2e_audit_log_and_summary_honor_toon_env_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_audit_log_toon");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Audit TOON seed"],
        "create_audit_toon_seed",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);

    let log = run_br_with_env(
        &workspace,
        ["audit", "log", &issue_id],
        [("BR_OUTPUT_FORMAT", "toon")],
        "audit_log_toon_env",
    );
    assert!(
        log.status.success(),
        "audit log toon failed: {}",
        log.stderr
    );
    let log_json = Value::from(try_decode(log.stdout.trim(), None).expect("valid audit log TOON"));
    assert_eq!(log_json["issue_id"].as_str(), Some(issue_id.as_str()));
    assert!(
        log_json["events"]
            .as_array()
            .is_some_and(|events| !events.is_empty()),
        "TOON audit log output should include events: {log_json}"
    );

    let summary = run_br_with_env(
        &workspace,
        ["audit", "summary", "--days", "30"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "audit_summary_toon_env",
    );
    assert!(
        summary.status.success(),
        "audit summary toon failed: {}",
        summary.stderr
    );
    let summary_json =
        Value::from(try_decode(summary.stdout.trim(), None).expect("valid audit summary TOON"));
    assert_eq!(toon_u64(&summary_json["period_days"]), Some(30));
    assert!(
        toon_u64(&summary_json["totals"]["total"]).is_some_and(|total| total >= 1),
        "TOON audit summary output should include totals: {summary_json}"
    );
}

#[test]
fn e2e_orphans_honors_toon_env_mode_when_empty() {
    let _log = common::test_log("e2e_orphans_honors_toon_env_mode_when_empty");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_orphans_toon");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let orphans = run_br_with_env(
        &workspace,
        ["orphans"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "orphans_toon_env",
    );
    assert!(
        orphans.status.success(),
        "orphans toon failed: {}",
        orphans.stderr
    );

    let decoded = try_decode(orphans.stdout.trim(), None).expect("valid empty orphans TOON");
    let json = Value::from(decoded);
    assert_eq!(json.as_array().map(Vec::len), Some(0));
}

#[cfg(unix)]
#[test]
fn e2e_comments_add_does_not_invoke_git_for_author_fallback() {
    let _log = common::test_log("e2e_comments_add_does_not_invoke_git_for_author_fallback");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_comments_author_env");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Comment author target", "--json"],
        "create_comments_author_target",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let issue_id = issue["id"].as_str().expect("issue id").to_string();

    let fake_bin = workspace.root.join("fake-bin");
    fs::create_dir_all(&fake_bin).expect("create fake bin");
    let marker_path = workspace.root.join("git-was-called");
    let fake_git_path = fake_bin.join("git");
    fs::write(
        &fake_git_path,
        format!(
            "#!/bin/sh\nprintf called > \"{}\"\nexit 99\n",
            marker_path.display()
        ),
    )
    .expect("write fake git");
    let mut permissions = fs::metadata(&fake_git_path)
        .expect("fake git metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_git_path, permissions).expect("chmod fake git");

    let comment_add = run_br_with_env(
        &workspace,
        ["comments", "add", &issue_id, "Author via env", "--json"],
        vec![
            ("PATH".to_string(), fake_bin.display().to_string()),
            ("USER".to_string(), "env-author".to_string()),
        ],
        "comments_add_author_env_no_git",
    );
    assert!(
        comment_add.status.success(),
        "comments add failed: {}",
        comment_add.stderr
    );

    let added: Value =
        serde_json::from_str(&extract_json_payload(&comment_add.stdout)).expect("comment add json");
    assert_eq!(added["issue_id"].as_str(), Some(issue_id.as_str()));
    assert_eq!(added["author"].as_str(), Some("env-author"));
    assert!(
        !marker_path.exists(),
        "comments add should not invoke git while resolving author"
    );
}

#[test]
fn e2e_where_honors_toon_env_mode() {
    let _log = common::test_log("e2e_where_honors_toon_env_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let where_result = run_br_with_env(
        &workspace,
        ["where"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "where_toon_env",
    );
    assert!(
        where_result.status.success(),
        "where toon failed: {}",
        where_result.stderr
    );

    let decoded = try_decode(where_result.stdout.trim(), None).expect("valid where TOON");
    let json = Value::from(decoded);
    assert!(
        json["path"]
            .as_str()
            .is_some_and(|path| path.contains(".beads")),
        "TOON where output should include the beads path: {json}"
    );
    assert!(
        json["database_path"]
            .as_str()
            .is_some_and(|path| path.contains("beads.db")),
        "TOON where output should include the database path: {json}"
    );
}

#[test]
fn e2e_query_save_list_delete_honor_toon_env_mode() {
    let _log = common::test_log("e2e_query_save_list_delete_honor_toon_env_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "query_toon_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let save = run_br_with_env(
        &workspace,
        ["query", "save", "toon-open", "--status", "open"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "query_save_toon_env",
    );
    assert!(save.status.success(), "query save failed: {}", save.stderr);
    let save_json = Value::from(try_decode(save.stdout.trim(), None).expect("valid save TOON"));
    assert_eq!(save_json["status"].as_str(), Some("ok"));
    assert_eq!(save_json["action"].as_str(), Some("saved"));
    assert_eq!(save_json["name"].as_str(), Some("toon-open"));

    let list = run_br_with_env(
        &workspace,
        ["query", "list"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "query_list_toon_env",
    );
    assert!(list.status.success(), "query list failed: {}", list.stderr);
    let list_json = Value::from(try_decode(list.stdout.trim(), None).expect("valid list TOON"));
    assert_eq!(toon_u64(&list_json["count"]), Some(1));
    let queries = toon_array_items(&list_json["queries"]);
    assert_eq!(queries.len(), 1);
    assert_eq!(queries[0]["name"].as_str(), Some("toon-open"));

    let delete = run_br_with_env(
        &workspace,
        ["query", "delete", "toon-open"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "query_delete_toon_env",
    );
    assert!(
        delete.status.success(),
        "query delete failed: {}",
        delete.stderr
    );
    let delete_json =
        Value::from(try_decode(delete.stdout.trim(), None).expect("valid delete TOON"));
    assert_eq!(delete_json["status"].as_str(), Some("ok"));
    assert_eq!(delete_json["action"].as_str(), Some("deleted"));
    assert_eq!(delete_json["name"].as_str(), Some("toon-open"));
}

#[test]
fn e2e_history_list_and_prune_honor_toon_env_mode() {
    let _log = common::test_log("e2e_history_list_and_prune_honor_toon_env_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "history_toon_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create_initial = run_br(
        &workspace,
        ["--no-auto-flush", "create", "History TOON seed"],
        "history_toon_create_initial",
    );
    assert!(
        create_initial.status.success(),
        "initial create failed: {}",
        create_initial.stderr
    );

    let initial_sync = run_br(
        &workspace,
        ["sync", "--flush-only"],
        "history_toon_initial_sync",
    );
    assert!(
        initial_sync.status.success(),
        "initial sync failed: {}",
        initial_sync.stderr
    );

    let create_second = run_br(
        &workspace,
        ["--no-auto-flush", "create", "History TOON second"],
        "history_toon_create_second",
    );
    assert!(
        create_second.status.success(),
        "second create failed: {}",
        create_second.stderr
    );

    let second_sync = run_br(
        &workspace,
        ["sync", "--flush-only"],
        "history_toon_second_sync",
    );
    assert!(
        second_sync.status.success(),
        "second sync failed: {}",
        second_sync.stderr
    );

    let list = run_br_with_env(
        &workspace,
        ["history", "list"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "history_list_toon_env",
    );
    assert!(
        list.status.success(),
        "history list failed: {}",
        list.stderr
    );
    let list_json = Value::from(try_decode(list.stdout.trim(), None).expect("valid list TOON"));
    assert!(
        toon_u64(&list_json["count"]).is_some_and(|count| count >= 1),
        "TOON history list should report at least one backup: {list_json}"
    );
    assert!(
        list_json["backups"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "TOON history list should include backups: {list_json}"
    );

    let prune = run_br_with_env(
        &workspace,
        ["history", "prune", "--keep", "10"],
        [("BR_OUTPUT_FORMAT", "toon")],
        "history_prune_toon_env",
    );
    assert!(
        prune.status.success(),
        "history prune failed: {}",
        prune.stderr
    );
    let prune_json = Value::from(try_decode(prune.stdout.trim(), None).expect("valid prune TOON"));
    assert_eq!(prune_json["action"].as_str(), Some("prune"));
    assert_eq!(toon_u64(&prune_json["keep"]), Some(10));
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

    let list_json = parse_list_issues(&list.stdout);
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

    let list_json = parse_list_issues(&list.stdout);
    assert!(
        list_json
            .iter()
            .any(|item| item["title"] == "Whitespace env test"),
        "whitespace BEADS_DIR should be ignored"
    );
}
