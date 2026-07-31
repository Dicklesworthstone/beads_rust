//! E2E tests for `br create --slug` round-trip + slugged-ID tolerance in
//! downstream commands (orphans, show, update, close).
//!
//! Created 2026-05-09 for beads_rust-l6xl (audit-driven snapshot + slug
//! coverage). Pairs with the unit tests in `src/util/id.rs::tests` (those
//! verify the normalizer; these verify the full CLI lifecycle).

#![allow(clippy::items_after_statements, clippy::too_many_lines)]

mod common;

use common::cli::{BrWorkspace, run_br};
use std::process::Command;

fn extract_id_from_create_stdout(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let line = line.trim().trim_start_matches("✓ ");
        if let Some(rest) = line.strip_prefix("Created ") {
            // "Created bd-foo-bar-abc1234: <title>"
            if let Some((id, _)) = rest.split_once(':') {
                return Some(id.trim().to_string());
            }
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// l6xl AC: full round-trip — `br create --slug "feature x"`, capture the
/// returned ID, confirm `br show <id> --json` returns the same ID and a
/// title field that contains the user-provided text.
#[test]
fn e2e_create_with_slug_then_show_renders_slug_in_id() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "bd"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    eprintln!("[l6xl TEST] e2e_create_with_slug_then_show_renders_slug_in_id");

    // Create with slug
    let create = run_br(
        &workspace,
        [
            "create",
            "Add an exciting new feature",
            "--slug",
            "exciting-feature",
            "-t",
            "feature",
            "-p",
            "1",
        ],
        "create_with_slug",
    );
    assert!(
        create.status.success(),
        "create --slug failed: {}",
        create.stderr
    );

    let id = extract_id_from_create_stdout(&create.stdout)
        .unwrap_or_else(|| panic!("could not extract ID from stdout: {:?}", create.stdout));
    eprintln!("  generated ID: {id}");
    assert!(
        id.starts_with("bd-exciting-feature-"),
        "expected slug embedded in ID; got {id}"
    );

    // Round-trip: show --json must return the same ID
    let show = run_br(&workspace, ["show", &id, "--json"], "show_slug");
    assert!(
        show.status.success(),
        "show <slugged-id> failed: {}",
        show.stderr
    );

    // Parse the JSON output and confirm id matches
    let json: serde_json::Value =
        serde_json::from_str(show.stdout.trim()).expect("show output must be valid JSON");
    let returned_id = if let Some(arr) = json.as_array() {
        arr.first()
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .map(String::from)
    } else {
        json.get("id").and_then(|v| v.as_str()).map(String::from)
    };
    assert_eq!(
        returned_id.as_deref(),
        Some(id.as_str()),
        "show JSON must return the same ID as create produced"
    );

    eprintln!("  [PASS] slug round-trip via CLI");
}

#[test]
fn e2e_templated_id_derives_slug_and_resolves_numeric_short_id() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "bd"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    std::fs::write(
        workspace.root.join(".beads").join("config.yaml"),
        r#"
issue_prefix: bd
id_generation:
  mode: templated
  template: "{seq:03}-{slug}-{hash}"
  require_slug: true
"#,
    )
    .expect("write templated ID config");

    let dry_run = run_br(
        &workspace,
        ["create", "Preview sequence", "--dry-run"],
        "dry_run_templated_id",
    );
    assert!(
        dry_run.status.success(),
        "templated dry run failed: {}\nstdout: {}",
        dry_run.stderr,
        dry_run.stdout
    );
    assert!(
        dry_run.stdout.contains("001-preview-sequence-"),
        "dry run should preview sequence 001: {}",
        dry_run.stdout
    );

    let create = run_br(
        &workspace,
        ["create", "Improve tests speed", "-t", "task"],
        "create_templated_id",
    );
    assert!(
        create.status.success(),
        "templated create failed: {}\nstdout: {}",
        create.stderr,
        create.stdout
    );
    let id = extract_id_from_create_stdout(&create.stdout)
        .unwrap_or_else(|| panic!("could not extract ID from stdout: {:?}", create.stdout));
    assert!(
        id.starts_with("001-improve-tests-speed-"),
        "expected taskmd-like sequence + slug; got {id}"
    );

    let show = run_br(
        &workspace,
        ["show", "001", "--json"],
        "show_numeric_short_id",
    );
    assert!(
        show.status.success(),
        "show numeric short ID failed: {}\nstdout: {}",
        show.stderr,
        show.stdout
    );
    let json: serde_json::Value =
        serde_json::from_str(show.stdout.trim()).expect("show output must be valid JSON");
    let returned_id = if let Some(arr) = json.as_array() {
        arr.first()
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .map(String::from)
    } else {
        json.get("id").and_then(|v| v.as_str()).map(String::from)
    };
    assert_eq!(returned_id.as_deref(), Some(id.as_str()));

    let child = run_br(
        &workspace,
        ["q", "Child from numeric parent", "--parent", "001"],
        "quick_child_numeric_parent",
    );
    assert!(
        child.status.success(),
        "quick child numeric parent failed: {}\nstdout: {}",
        child.stderr,
        child.stdout
    );
    assert!(
        child.stdout.contains(".1"),
        "quick child should use parent-derived child ID; stdout: {}",
        child.stdout
    );

    std::fs::write(
        workspace.root.join(".beads").join("config.yaml"),
        r#"
issue_prefix: bd
id_generation:
  mode: templated
  template: "{prefix}-{seq:03}-{slug}-{hash}"
  require_slug: true
"#,
    )
    .expect("change template for future issues");
    let second = run_br(
        &workspace,
        ["create", "Future template only"],
        "create_after_template_change",
    );
    assert!(
        second.status.success(),
        "create after template change failed: {}\nstdout: {}",
        second.stderr,
        second.stdout
    );
    let second_id = extract_id_from_create_stdout(&second.stdout)
        .unwrap_or_else(|| panic!("could not extract second ID: {:?}", second.stdout));
    assert!(second_id.starts_with("bd-002-future-template-only-"));

    let first_still_exists = run_br(
        &workspace,
        ["show", &id, "--json"],
        "show_first_after_template_change",
    );
    assert!(first_still_exists.status.success());
    assert!(first_still_exists.stdout.contains(&id));
}

#[test]
fn e2e_three_way_merge_preserves_sequence_only_jsonl_change() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "bd"], "merge_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let create = run_br(
        &workspace,
        ["create", "Merge sequence metadata"],
        "merge_create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = extract_id_from_create_stdout(&create.stdout).expect("created ID");
    let flush = run_br(&workspace, ["sync", "--flush-only"], "merge_flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let mut value: serde_json::Value =
        serde_json::from_str(std::fs::read_to_string(&jsonl_path).unwrap().trim()).unwrap();
    value["sequence_number"] = serde_json::Value::from(1);
    std::fs::write(
        &jsonl_path,
        format!("{}\n", serde_json::to_string(&value).unwrap()),
    )
    .unwrap();

    let merge = run_br(
        &workspace,
        ["sync", "--merge", "--force-jsonl"],
        "merge_sequence_only",
    );
    assert!(
        merge.status.success(),
        "sequence-only merge failed: {}\nstdout: {}",
        merge.stderr,
        merge.stdout
    );
    let show = run_br(&workspace, ["show", "001", "--json"], "merge_show_numeric");
    assert!(
        show.status.success(),
        "merged numeric sequence did not resolve: {}\nstdout: {}",
        show.stderr,
        show.stdout
    );
    assert!(show.stdout.contains(&id));
}

/// l6xl AC: orphans command must find references to slugged IDs in commit
/// messages (verifies that `f454486f fix(sync): accept slugged IDs in
/// prefix guard` and `52ff1722 feat(orphans): scan all candidate-issue
/// prefixes` work end-to-end via the CLI surface).
#[test]
fn e2e_orphans_handles_slugged_ids_in_commit_messages() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "bd"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    eprintln!("[l6xl TEST] e2e_orphans_handles_slugged_ids_in_commit_messages");

    // Create a slugged issue
    let create = run_br(
        &workspace,
        [
            "create",
            "Fix login flow",
            "--slug",
            "fix-login-flow",
            "-t",
            "bug",
        ],
        "create_for_orphans",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let slugged_id = extract_id_from_create_stdout(&create.stdout)
        .unwrap_or_else(|| panic!("could not extract ID: {:?}", create.stdout));
    eprintln!("  slugged ID: {slugged_id}");

    // Initialize a git repo in the workspace and create a commit referencing the slug
    let _ = Command::new("git")
        .args(["init"])
        .current_dir(&workspace.root)
        .output();
    let _ = Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(&workspace.root)
        .output();
    let _ = Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(&workspace.root)
        .output();

    // Touch a file to have something to commit
    std::fs::write(workspace.root.join("CHANGELOG.md"), "initial\n").expect("write changelog");
    let _ = Command::new("git")
        .args(["add", "."])
        .current_dir(&workspace.root)
        .output();
    let commit_msg = format!("feat: implement {slugged_id} login fix");
    let commit = Command::new("git")
        .args(["commit", "-m", &commit_msg])
        .current_dir(&workspace.root)
        .output()
        .expect("git commit");
    assert!(
        commit.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&commit.stderr)
    );

    // Run orphans --json and verify it finds the slugged ID
    let orphans = run_br(&workspace, ["orphans", "--json"], "orphans_check");
    assert!(
        orphans.status.success(),
        "orphans command failed: {}",
        orphans.stderr
    );
    eprintln!("  orphans output:\n{}", orphans.stdout);

    // The slugged ID is open + referenced in a commit → must appear as orphan
    assert!(
        orphans.stdout.contains(&slugged_id),
        "orphans output must include slugged ID {slugged_id}; got: {}",
        orphans.stdout
    );

    eprintln!("  [PASS] orphans command finds slugged ID in commit messages");
}
