mod common;

use beads_rust::model::{Issue, IssueType, Priority, Status};
use beads_rust::storage::SqliteStorage;
use beads_rust::sync::{auto_flush, compute_jsonl_hash, read_issues_from_jsonl};
use chrono::Utc;
use common::cli::{BrWorkspace, parse_created_id, run_br};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn make_issue(id: &str) -> Issue {
    Issue {
        id: id.to_string(),
        title: "Test Issue".to_string(),
        status: Status::Open,
        priority: Priority::MEDIUM,
        issue_type: IssueType::Task,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        ..Default::default()
    }
}

// `update_issue` always advances `updated_at`, so a change-then-revert pair is
// still a real JSONL content change. This remains ignored until the integration
// surface has a supported way to reproduce a byte-identical dirty marker.
#[test]
#[ignore = "integration test cannot mark an issue dirty without changing JSONL bytes"]
fn test_auto_flush_optimizes_no_content_change() {
    let temp_dir = TempDir::new().unwrap();
    let beads_dir = temp_dir.path().join(".beads");
    fs::create_dir(&beads_dir).unwrap();
    let db_path = beads_dir.join("beads.db");
    let jsonl_path = beads_dir.join("issues.jsonl");

    let mut storage = SqliteStorage::open(&db_path).unwrap();

    // 1. Create an issue
    let issue = make_issue("bd-1");
    storage.create_issue(&issue, "tester").unwrap();

    // 2. First auto-flush (should export)
    let result = auto_flush(&mut storage, &beads_dir, &jsonl_path, false).unwrap();
    assert!(result.flushed, "First flush should happen");
    assert_eq!(result.exported_count, 1);

    // 3. Mark issue dirty effectively WITHOUT changing content
    // We do this by changing it and changing it back.
    // NOTE: This relies on the fact that we haven't exported the intermediate state.

    // Change title
    let update_change = beads_rust::storage::IssueUpdate {
        title: Some("Changed Title".to_string()),
        ..Default::default()
    };
    storage
        .update_issue("bd-1", &update_change, "tester")
        .unwrap();

    // Revert title
    let update_revert = beads_rust::storage::IssueUpdate {
        title: Some("Test Issue".to_string()),
        ..Default::default()
    };
    storage
        .update_issue("bd-1", &update_revert, "tester")
        .unwrap();

    // Verify it is dirty
    let dirty_ids = storage.get_dirty_issue_ids().unwrap();
    assert_eq!(dirty_ids.len(), 1, "Issue should be dirty after updates");

    // 4. Second auto-flush should now skip the rewrite because the exported JSONL
    // would be byte-identical.
    let result = auto_flush(&mut storage, &beads_dir, &jsonl_path, false).unwrap();

    assert!(
        !result.flushed,
        "Auto-flush should skip a no-op rewrite when the JSONL would be unchanged"
    );

    // And dirty flags should be cleared
    let dirty_ids = storage.get_dirty_issue_ids().unwrap();
    assert!(dirty_ids.is_empty(), "Dirty flags should be cleared");
}

#[test]
fn test_auto_flush_flush_on_label_change() {
    let temp_dir = TempDir::new().unwrap();
    let beads_dir = temp_dir.path().join(".beads");
    fs::create_dir(&beads_dir).unwrap();
    let db_path = beads_dir.join("beads.db");
    let jsonl_path = beads_dir.join("issues.jsonl");

    let mut storage = SqliteStorage::open(&db_path).unwrap();

    // 1. Create an issue
    let issue = make_issue("bd-1");
    storage.create_issue(&issue, "tester").unwrap();

    // 2. First auto-flush
    let result = auto_flush(&mut storage, &beads_dir, &jsonl_path, false).unwrap();
    assert!(result.flushed);

    // 3. Add a label
    storage.add_label("bd-1", "bug", "tester").unwrap();

    // Verify dirty
    let dirty_ids = storage.get_dirty_issue_ids().unwrap();
    assert_eq!(dirty_ids.len(), 1);

    // 4. Second auto-flush - SHOULD FLUSH because label was added
    let result = auto_flush(&mut storage, &beads_dir, &jsonl_path, false).unwrap();

    // This assertion will FAIL if my optimization is active and flawed
    assert!(result.flushed, "Should flush when label is added");
}

#[test]
fn test_auto_flush_uses_resolved_jsonl_path() {
    let temp_dir = TempDir::new().unwrap();
    let beads_dir = temp_dir.path().join(".beads");
    fs::create_dir(&beads_dir).unwrap();
    let db_path = beads_dir.join("beads.db");
    let custom_jsonl_path = temp_dir.path().join("custom-issues.jsonl");

    let mut storage = SqliteStorage::open(&db_path).unwrap();
    storage.create_issue(&make_issue("bd-1"), "tester").unwrap();

    // A JSONL path outside `.beads/` requires `allow_external_jsonl = true`
    // — otherwise export refuses with "Path is outside the beads directory"
    // as a safety check against wayward writes during refactors.
    let result = auto_flush(&mut storage, &beads_dir, &custom_jsonl_path, true).unwrap();

    assert!(result.flushed);
    assert!(custom_jsonl_path.exists());
    assert!(!beads_dir.join("issues.jsonl").exists());
}

#[test]
fn test_auto_flush_preserves_unrelated_existing_jsonl_lines() {
    let temp_dir = TempDir::new().unwrap();
    let beads_dir = temp_dir.path().join(".beads");
    fs::create_dir(&beads_dir).unwrap();
    let db_path = beads_dir.join("beads.db");
    let jsonl_path = beads_dir.join("issues.jsonl");

    let mut storage = SqliteStorage::open(&db_path).unwrap();
    storage.create_issue(&make_issue("bd-1"), "tester").unwrap();
    auto_flush(&mut storage, &beads_dir, &jsonl_path, false).unwrap();

    let extra_issue = make_issue("bd-extra");
    let mut contents = fs::read_to_string(&jsonl_path).unwrap();
    contents.push_str(&format!(
        "{}\n",
        serde_json::to_string(&extra_issue).unwrap()
    ));
    fs::write(&jsonl_path, contents).unwrap();

    storage
        .update_issue(
            "bd-1",
            &beads_rust::storage::IssueUpdate {
                title: Some("Updated".to_string()),
                ..Default::default()
            },
            "tester",
        )
        .unwrap();

    let result = auto_flush(&mut storage, &beads_dir, &jsonl_path, false).unwrap();
    assert!(result.flushed);

    let issues = beads_rust::sync::read_issues_from_jsonl(&jsonl_path).unwrap();
    let ids = issues
        .iter()
        .map(|issue| issue.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["bd-1", "bd-extra"]);
    assert_eq!(
        issues
            .iter()
            .find(|issue| issue.id == "bd-1")
            .unwrap()
            .title,
        "Updated"
    );
}

fn read_jsonl_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

fn issue_id_from_jsonl_line(line: &str) -> String {
    serde_json::from_str::<serde_json::Value>(line)
        .unwrap()
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap()
        .to_string()
}

fn lines_by_issue_id(lines: &[String]) -> BTreeMap<String, String> {
    lines
        .iter()
        .map(|line| (issue_id_from_jsonl_line(line), line.clone()))
        .collect()
}

fn changed_issue_ids(before: &[String], after: &[String]) -> BTreeSet<String> {
    let before_by_id = lines_by_issue_id(before);
    let after_by_id = lines_by_issue_id(after);
    before_by_id
        .keys()
        .chain(after_by_id.keys())
        .filter(|id| before_by_id.get(*id) != after_by_id.get(*id))
        .cloned()
        .collect()
}

fn positional_line_churn(before: &[String], after: &[String]) -> usize {
    let common_changed = before
        .iter()
        .zip(after)
        .filter(|(left, right)| left != right)
        .count();
    common_changed + before.len().abs_diff(after.len())
}

fn assert_jsonl_is_valid_and_acyclic(workspace: &BrWorkspace, label: &str) {
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    read_issues_from_jsonl(&jsonl_path).unwrap();
    let cycles = run_br(
        workspace,
        ["dep", "cycles", "--json"],
        &format!("{label}_dep_cycles"),
    );
    assert!(
        cycles.status.success(),
        "dependency-cycle validation failed for {label}: stdout={} stderr={}",
        cycles.stdout,
        cycles.stderr
    );
}

fn assert_bounded_jsonl_diff(
    workspace: &BrWorkspace,
    label: &str,
    before: &[String],
    expected_changed_ids: &[String],
    max_line_churn: usize,
) {
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let after = read_jsonl_lines(&jsonl_path);
    let observed_changed_ids = changed_issue_ids(before, &after);
    let expected_changed_ids = expected_changed_ids.iter().cloned().collect::<BTreeSet<_>>();
    let line_churn = positional_line_churn(before, &after);
    let line_count = after.len();
    let content_hash = compute_jsonl_hash(&jsonl_path).unwrap();

    assert_eq!(
        observed_changed_ids, expected_changed_ids,
        "changed issue set mismatch for {label}; temp tracker hash after={content_hash}, line_count={line_count}, line_churn={line_churn}"
    );
    assert!(
        line_churn <= max_line_churn,
        "JSONL rewrite was not bounded for {label}; temp tracker hash after={content_hash}, line_count={line_count}, line_churn={line_churn}, expected_changed_ids={expected_changed_ids:?}, observed_changed_ids={observed_changed_ids:?}"
    );
    assert_jsonl_is_valid_and_acyclic(workspace, label);
}

#[test]
fn e2e_auto_flush_single_mutations_preserve_bounded_jsonl_diff_after_import() {
    let _log =
        common::test_log("e2e_auto_flush_single_mutations_preserve_bounded_jsonl_diff_after_import");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "bounded_diff_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create_a = run_br(&workspace, ["create", "Alpha"], "bounded_diff_create_a");
    assert!(
        create_a.status.success(),
        "create alpha failed: {}",
        create_a.stderr
    );
    let alpha_id = parse_created_id(&create_a.stdout);

    let create_b = run_br(&workspace, ["create", "Beta"], "bounded_diff_create_b");
    assert!(
        create_b.status.success(),
        "create beta failed: {}",
        create_b.stderr
    );
    let beta_id = parse_created_id(&create_b.stdout);

    let create_c = run_br(&workspace, ["create", "Gamma"], "bounded_diff_create_c");
    assert!(
        create_c.status.success(),
        "create gamma failed: {}",
        create_c.stderr
    );
    let gamma_id = parse_created_id(&create_c.stdout);

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let mut imported_lines = read_jsonl_lines(&jsonl_path);
    imported_lines.reverse();
    fs::write(&jsonl_path, format!("{}\n", imported_lines.join("\n"))).unwrap();

    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--force", "--json"],
        "bounded_diff_import_reordered_jsonl",
    );
    assert!(
        import.status.success(),
        "import reordered JSONL failed: stdout={} stderr={}",
        import.stdout,
        import.stderr
    );
    assert_eq!(
        read_jsonl_lines(&jsonl_path),
        imported_lines,
        "import-only should not rewrite the reordered JSONL fixture"
    );

    let before_update = read_jsonl_lines(&jsonl_path);
    let update = run_br(
        &workspace,
        ["update", &alpha_id, "--title", "Alpha renamed", "--json"],
        "bounded_diff_update",
    );
    assert!(
        update.status.success(),
        "update failed: stdout={} stderr={}",
        update.stdout,
        update.stderr
    );
    assert_bounded_jsonl_diff(
        &workspace,
        "update",
        &before_update,
        std::slice::from_ref(&alpha_id),
        1,
    );

    let before_comment = read_jsonl_lines(&jsonl_path);
    let comment = run_br(
        &workspace,
        ["comments", "add", &alpha_id, "bounded comment", "--json"],
        "bounded_diff_comment",
    );
    assert!(
        comment.status.success(),
        "comment add failed: stdout={} stderr={}",
        comment.stdout,
        comment.stderr
    );
    assert_bounded_jsonl_diff(
        &workspace,
        "comment add",
        &before_comment,
        std::slice::from_ref(&alpha_id),
        1,
    );

    let before_dep = read_jsonl_lines(&jsonl_path);
    let dep = run_br(
        &workspace,
        ["dep", "add", &alpha_id, &beta_id, "--type", "related", "--json"],
        "bounded_diff_dep",
    );
    assert!(
        dep.status.success(),
        "dep add failed: stdout={} stderr={}",
        dep.stdout,
        dep.stderr
    );
    assert_bounded_jsonl_diff(
        &workspace,
        "dep add",
        &before_dep,
        std::slice::from_ref(&alpha_id),
        1,
    );

    let before_create = read_jsonl_lines(&jsonl_path);
    let create_new = run_br(&workspace, ["create", "Delta"], "bounded_diff_create_delta");
    assert!(
        create_new.status.success(),
        "create delta failed: stdout={} stderr={}",
        create_new.stdout,
        create_new.stderr
    );
    let delta_id = parse_created_id(&create_new.stdout);
    assert_bounded_jsonl_diff(
        &workspace,
        "create",
        &before_create,
        std::slice::from_ref(&delta_id),
        1,
    );

    assert!(
        [alpha_id, beta_id, gamma_id, delta_id]
            .iter()
            .all(|id| !id.trim().is_empty()),
        "all command-created ids must be present"
    );
}
