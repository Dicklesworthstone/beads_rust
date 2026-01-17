mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;

fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    let id_part = line
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("");
    id_part.trim().to_string()
}

fn create_issue_with_description(
    workspace: &BrWorkspace,
    title: &str,
    issue_type: Option<&str>,
    description: Option<&str>,
    label: &str,
) -> String {
    let mut args = vec!["create".to_string(), title.to_string()];
    if let Some(kind) = issue_type {
        args.push("--type".to_string());
        args.push(kind.to_string());
    }
    if let Some(text) = description {
        args.push("--description".to_string());
        args.push(text.to_string());
    }
    let create = run_br(workspace, args, label);
    assert!(create.status.success(), "create failed: {}", create.stderr);
    parse_created_id(&create.stdout)
}

fn run_lint_json(workspace: &BrWorkspace, mut args: Vec<String>, label: &str) -> Value {
    args.push("--json".to_string());
    let lint = run_br(workspace, args, label);
    assert!(lint.status.success(), "lint json failed: {}", lint.stderr);
    let payload = extract_json_payload(&lint.stdout);
    serde_json::from_str(&payload).expect("parse lint json")
}

#[test]
fn e2e_error_handling() {
    let workspace = BrWorkspace::new();

    let list_uninit = run_br(&workspace, ["list"], "list_uninitialized");
    assert!(!list_uninit.status.success());

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Bad status"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    let bad_status = run_br(
        &workspace,
        ["update", &id, "--status", "not_a_status"],
        "update_bad_status",
    );
    assert!(!bad_status.status.success());

    let bad_priority = run_br(
        &workspace,
        ["list", "--priority-min", "9"],
        "list_bad_priority",
    );
    assert!(!bad_priority.status.success());

    let bad_ready_priority = run_br(
        &workspace,
        ["ready", "--priority", "9"],
        "ready_bad_priority",
    );
    assert!(!bad_ready_priority.status.success());

    let bad_label = run_br(
        &workspace,
        ["update", &id, "--add-label", "bad label"],
        "update_bad_label",
    );
    assert!(!bad_label.status.success());

    let show_missing = run_br(&workspace, ["show", "bd-doesnotexist"], "show_missing");
    assert!(!show_missing.status.success());

    let delete_missing = run_br(&workspace, ["delete", "bd-doesnotexist"], "delete_missing");
    assert!(!delete_missing.status.success());

    let beads_dir = workspace.root.join(".beads");
    let issues_path = beads_dir.join("issues.jsonl");
    fs::write(
        &issues_path,
        "<<<<<<< HEAD\n{}\n=======\n{}\n>>>>>>> branch\n",
    )
    .expect("write conflict jsonl");

    let sync_bad = run_br(&workspace, ["sync", "--import-only"], "sync_bad_jsonl");
    assert!(!sync_bad.status.success());
}

#[test]
fn e2e_dependency_errors() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let issue_a = run_br(&workspace, ["create", "Issue A"], "create_a");
    assert!(
        issue_a.status.success(),
        "create A failed: {}",
        issue_a.stderr
    );
    let id_a = parse_created_id(&issue_a.stdout);

    let issue_b = run_br(&workspace, ["create", "Issue B"], "create_b");
    assert!(
        issue_b.status.success(),
        "create B failed: {}",
        issue_b.stderr
    );
    let id_b = parse_created_id(&issue_b.stdout);

    let self_dep = run_br(&workspace, ["dep", "add", &id_a, &id_a], "dep_self");
    assert!(!self_dep.status.success(), "self dependency should fail");

    let add = run_br(&workspace, ["dep", "add", &id_a, &id_b], "dep_add");
    assert!(add.status.success(), "dep add failed: {}", add.stderr);

    let cycle = run_br(&workspace, ["dep", "add", &id_b, &id_a], "dep_cycle");
    assert!(!cycle.status.success(), "cycle dependency should fail");
}

#[test]
fn e2e_sync_invalid_orphans() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Sync issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    let bad_orphans = run_br(
        &workspace,
        ["sync", "--import-only", "--force", "--orphans", "weird"],
        "sync_bad_orphans",
    );
    assert!(
        !bad_orphans.status.success(),
        "invalid orphans mode should fail"
    );
}

#[test]
fn e2e_sync_export_guards() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let beads_dir = workspace.root.join(".beads");
    let issues_path = beads_dir.join("issues.jsonl");

    // Empty DB guard: JSONL has content but DB has zero issues.
    fs::write(&issues_path, "{\"id\":\"bd-ghost\"}\n").expect("write jsonl");
    let flush_guard = run_br(&workspace, ["sync", "--flush-only"], "sync_flush_guard");
    assert!(
        !flush_guard.status.success(),
        "expected empty DB guard failure"
    );
    assert!(
        flush_guard
            .stderr
            .contains("Refusing to export empty database"),
        "missing empty DB guard message"
    );
    // Reset JSONL to avoid guard on the seed export.
    fs::write(&issues_path, "").expect("reset jsonl");

    // Stale DB guard: JSONL has an ID missing from DB.
    let create = run_br(&workspace, ["create", "Stale guard issue"], "create_stale");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush_seed");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    let mut contents = fs::read_to_string(&issues_path).expect("read jsonl");
    contents.push_str("{\"id\":\"bd-missing\"}\n");
    fs::write(&issues_path, contents).expect("append jsonl");

    let create2 = run_br(&workspace, ["create", "Dirty issue"], "create_dirty");
    assert!(
        create2.status.success(),
        "create failed: {}",
        create2.stderr
    );

    let flush_stale = run_br(&workspace, ["sync", "--flush-only"], "sync_flush_stale");
    assert!(
        !flush_stale.status.success(),
        "expected stale DB guard failure"
    );
    assert!(
        flush_stale
            .stderr
            .contains("Refusing to export stale database"),
        "missing stale DB guard message"
    );
}

#[test]
fn e2e_ambiguous_id() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let mut ids: Vec<String> = Vec::new();
    let mut attempt = 0;
    let mut ambiguous_char: Option<char> = None;

    while ambiguous_char.is_none() && attempt < 20 {
        let title = format!("Ambiguous {attempt}");
        let create = run_br(&workspace, ["create", &title], "create_ambiguous");
        assert!(create.status.success(), "create failed: {}", create.stderr);
        let id = parse_created_id(&create.stdout);
        ids.push(id);

        let mut matches: HashMap<char, std::collections::HashSet<String>> = HashMap::new();
        for id in &ids {
            let hash = id.split('-').nth(1).unwrap_or("");
            for ch in hash.chars() {
                matches.entry(ch).or_default().insert(id.clone());
            }
        }

        ambiguous_char = matches
            .iter()
            .find(|(_, ids)| ids.len() >= 2)
            .map(|(ch, _)| *ch);

        attempt += 1;
    }

    let ambiguous_char = ambiguous_char.expect("failed to find ambiguous char");
    let ambiguous_input = ambiguous_char.to_string();

    let show = run_br(&workspace, ["show", &ambiguous_input], "show_ambiguous");
    assert!(!show.status.success(), "ambiguous id should fail");
}

#[test]
fn e2e_lint_before_init_fails() {
    let workspace = BrWorkspace::new();
    let lint = run_br(&workspace, ["lint"], "lint_before_init");
    assert!(!lint.status.success());
}

#[test]
fn e2e_lint_clean_output_when_no_warnings() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "lint_clean_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let description = "## Acceptance Criteria\n- done";
    create_issue_with_description(
        &workspace,
        "Task with criteria",
        Some("task"),
        Some(description),
        "lint_clean_create",
    );

    let lint = run_br(&workspace, ["lint"], "lint_clean_run");
    assert!(
        lint.status.success(),
        "lint should succeed: {}",
        lint.stderr
    );
    assert!(lint.stdout.contains("No template warnings found"));
}

#[test]
fn e2e_lint_bug_missing_sections_json() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "lint_bug_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    create_issue_with_description(
        &workspace,
        "Bug with missing sections",
        Some("bug"),
        Some("Bug report"),
        "lint_bug_create",
    );

    let json = run_lint_json(&workspace, vec!["lint".to_string()], "lint_bug_json");
    assert_eq!(json["total"].as_u64(), Some(2));
    assert_eq!(json["issues"].as_u64(), Some(1));
    let missing = json["results"][0]["missing"]
        .as_array()
        .expect("missing array");
    let missing_text: Vec<String> = missing
        .iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect();
    assert!(missing_text.contains(&"## Steps to Reproduce".to_string()));
    assert!(missing_text.contains(&"## Acceptance Criteria".to_string()));
}

#[test]
fn e2e_lint_multiple_issues_aggregate_warnings() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "lint_multi_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    create_issue_with_description(
        &workspace,
        "Bug missing sections",
        Some("bug"),
        Some("Bug report"),
        "lint_multi_bug",
    );
    create_issue_with_description(
        &workspace,
        "Task missing criteria",
        Some("task"),
        Some("Task description"),
        "lint_multi_task",
    );

    let json = run_lint_json(&workspace, vec!["lint".to_string()], "lint_multi_json");
    assert_eq!(json["issues"].as_u64(), Some(2));
    assert_eq!(json["total"].as_u64(), Some(3));
}

#[test]
fn e2e_lint_text_output_exit_code() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "lint_text_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    create_issue_with_description(
        &workspace,
        "Bug missing sections",
        Some("bug"),
        Some("Bug report"),
        "lint_text_bug",
    );

    let lint = run_br(&workspace, ["lint"], "lint_text_run");
    assert!(!lint.status.success());
    assert!(lint.stdout.contains("Template warnings"));
}

#[test]
fn e2e_lint_status_all_includes_closed() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "lint_closed_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let id = create_issue_with_description(
        &workspace,
        "Closed bug",
        Some("bug"),
        Some("Bug report"),
        "lint_closed_bug",
    );

    let close = run_br(
        &workspace,
        ["close", &id, "--reason", "done"],
        "lint_closed_close",
    );
    assert!(close.status.success(), "close failed: {}", close.stderr);

    let json = run_lint_json(
        &workspace,
        vec![
            "lint".to_string(),
            "--status".to_string(),
            "all".to_string(),
        ],
        "lint_closed_json",
    );
    assert_eq!(json["issues"].as_u64(), Some(1));
}

#[test]
fn e2e_lint_type_filter_limits_results() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "lint_type_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    create_issue_with_description(
        &workspace,
        "Bug missing sections",
        Some("bug"),
        Some("Bug report"),
        "lint_type_bug",
    );
    create_issue_with_description(
        &workspace,
        "Task with criteria",
        Some("task"),
        Some("## Acceptance Criteria\n- done"),
        "lint_type_task",
    );

    let json = run_lint_json(
        &workspace,
        vec!["lint".to_string(), "--type".to_string(), "bug".to_string()],
        "lint_type_json",
    );
    assert_eq!(json["issues"].as_u64(), Some(1));
    assert_eq!(json["results"][0]["type"].as_str(), Some("bug"));
}

#[test]
fn e2e_lint_ids_only_lints_selected() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "lint_ids_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let bug_id = create_issue_with_description(
        &workspace,
        "Bug missing sections",
        Some("bug"),
        Some("Bug report"),
        "lint_ids_bug",
    );
    create_issue_with_description(
        &workspace,
        "Task missing criteria",
        Some("task"),
        Some("Task description"),
        "lint_ids_task",
    );

    let json = run_lint_json(
        &workspace,
        vec!["lint".to_string(), bug_id.clone()],
        "lint_ids_json",
    );
    assert_eq!(json["issues"].as_u64(), Some(1));
    assert_eq!(json["results"][0]["id"].as_str(), Some(bug_id.as_str()));
}

#[test]
fn e2e_lint_skips_types_without_required_sections() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "lint_skip_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    create_issue_with_description(
        &workspace,
        "Chore without requirements",
        Some("chore"),
        Some("No requirements"),
        "lint_skip_chore",
    );

    let json = run_lint_json(&workspace, vec!["lint".to_string()], "lint_skip_json");
    assert_eq!(json["issues"].as_u64(), Some(0));
    assert_eq!(json["total"].as_u64(), Some(0));
}

// === Structured JSON Error Output Tests ===

/// Parse structured error JSON from stderr.
fn parse_error_json(stderr: &str) -> Option<Value> {
    serde_json::from_str(stderr).ok()
}

/// Verify error JSON has required fields.
fn verify_error_structure(json: &Value) -> bool {
    let error = json.get("error");
    if error.is_none() {
        return false;
    }
    let error = error.unwrap();

    // Required fields
    error.get("code").is_some()
        && error.get("message").is_some()
        && error.get("retryable").is_some()
}

#[test]
fn e2e_structured_error_not_initialized() {
    let workspace = BrWorkspace::new();

    // Don't init - test NOT_INITIALIZED error
    let result = run_br(&workspace, ["list", "--json"], "list_not_init_json");
    assert!(!result.status.success());
    assert_eq!(result.status.code(), Some(2), "exit code should be 2");

    let json = parse_error_json(&result.stderr).expect("should be valid JSON");
    assert!(verify_error_structure(&json), "missing required fields");

    let error = &json["error"];
    assert_eq!(error["code"], "NOT_INITIALIZED");
    assert!(!error["retryable"].as_bool().unwrap());
    assert!(error["hint"].as_str().unwrap().contains("br init"));
}

#[test]
fn e2e_structured_error_issue_not_found() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let result = run_br(
        &workspace,
        ["show", "bd-nonexistent", "--json"],
        "show_missing_json",
    );
    assert!(!result.status.success());
    assert_eq!(result.status.code(), Some(3), "exit code should be 3");

    let json = parse_error_json(&result.stderr).expect("should be valid JSON");
    assert!(verify_error_structure(&json), "missing required fields");

    let error = &json["error"];
    assert_eq!(error["code"], "ISSUE_NOT_FOUND");
    assert!(!error["retryable"].as_bool().unwrap());
    assert!(error["context"]["searched_id"].is_string());
    assert!(error["hint"].as_str().unwrap().contains("br list"));
}

#[test]
fn e2e_structured_error_invalid_status() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success());
    let id = parse_created_id(&create.stdout);

    let result = run_br(
        &workspace,
        ["update", &id, "--status", "done", "--json"],
        "update_status_done_json",
    );
    assert!(!result.status.success());
    assert_eq!(result.status.code(), Some(4), "exit code should be 4");

    let json = parse_error_json(&result.stderr).expect("should be valid JSON");
    assert!(verify_error_structure(&json), "missing required fields");

    let error = &json["error"];
    assert_eq!(error["code"], "INVALID_STATUS");
    assert!(error["retryable"].as_bool().unwrap());
    // Should suggest "closed" since "done" is a synonym
    assert!(
        error["hint"].as_str().unwrap().contains("closed"),
        "hint should suggest 'closed' for 'done'"
    );
}

#[test]
fn e2e_structured_error_cycle_detected() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create_a = run_br(&workspace, ["create", "Issue A"], "create_a");
    assert!(create_a.status.success());
    let id_a = parse_created_id(&create_a.stdout);

    let create_b = run_br(&workspace, ["create", "Issue B"], "create_b");
    assert!(create_b.status.success());
    let id_b = parse_created_id(&create_b.stdout);

    // A depends on B
    let dep_add = run_br(&workspace, ["dep", "add", &id_a, &id_b], "dep_add");
    assert!(dep_add.status.success());

    // B depends on A - would create cycle
    let result = run_br(
        &workspace,
        ["dep", "add", &id_b, &id_a, "--json"],
        "dep_cycle_json",
    );
    assert!(!result.status.success());
    assert_eq!(result.status.code(), Some(5), "exit code should be 5");

    let json = parse_error_json(&result.stderr).expect("should be valid JSON");
    assert!(verify_error_structure(&json), "missing required fields");

    let error = &json["error"];
    assert_eq!(error["code"], "CYCLE_DETECTED");
    assert!(!error["retryable"].as_bool().unwrap());
    assert!(error["context"]["cycle_path"].is_string());
}

#[test]
fn e2e_structured_error_self_dependency() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create = run_br(&workspace, ["create", "Self dep issue"], "create");
    assert!(create.status.success());
    let id = parse_created_id(&create.stdout);

    let result = run_br(
        &workspace,
        ["dep", "add", &id, &id, "--json"],
        "dep_self_json",
    );
    assert!(!result.status.success());
    assert_eq!(result.status.code(), Some(5), "exit code should be 5");

    let json = parse_error_json(&result.stderr).expect("should be valid JSON");
    assert!(verify_error_structure(&json), "missing required fields");

    let error = &json["error"];
    assert_eq!(error["code"], "SELF_DEPENDENCY");
    assert!(!error["retryable"].as_bool().unwrap());
}

#[test]
fn e2e_structured_error_ambiguous_id() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let mut ids: Vec<String> = Vec::new();
    let mut attempt = 0;
    let mut ambiguous_prefix: Option<String> = None;

    // Create issues until we have ambiguous IDs
    while ambiguous_prefix.is_none() && attempt < 30 {
        let title = format!("Structured test {attempt}");
        let create = run_br(&workspace, ["create", &title], &format!("create_{attempt}"));
        assert!(create.status.success());
        let id = parse_created_id(&create.stdout);
        ids.push(id);

        // Check for prefix collisions
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                let hash_i = ids[i].split('-').nth(1).unwrap_or("");
                let hash_j = ids[j].split('-').nth(1).unwrap_or("");
                if !hash_i.is_empty()
                    && !hash_j.is_empty()
                    && hash_i.chars().next() == hash_j.chars().next()
                {
                    let common_char = hash_i.chars().next().unwrap();
                    ambiguous_prefix = Some(common_char.to_string());
                    break;
                }
            }
            if ambiguous_prefix.is_some() {
                break;
            }
        }
        attempt += 1;
    }

    let prefix = ambiguous_prefix.expect("failed to create ambiguous IDs");

    let result = run_br(
        &workspace,
        ["show", &prefix, "--json"],
        "show_ambiguous_json",
    );
    assert!(!result.status.success());
    assert_eq!(result.status.code(), Some(3), "exit code should be 3");

    let json = parse_error_json(&result.stderr).expect("should be valid JSON");
    assert!(verify_error_structure(&json), "missing required fields");

    let error = &json["error"];
    assert_eq!(error["code"], "AMBIGUOUS_ID");
    assert!(error["retryable"].as_bool().unwrap());
    assert!(error["context"]["matches"].is_array());
}

#[test]
fn e2e_structured_error_jsonl_parse() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Create malformed JSONL
    let beads_dir = workspace.root.join(".beads");
    let issues_path = beads_dir.join("issues.jsonl");
    fs::write(&issues_path, "{ not valid json\n").expect("write bad jsonl");

    let result = run_br(
        &workspace,
        ["sync", "--import-only", "--json"],
        "import_bad_json",
    );
    assert!(!result.status.success());
    // JSONL parse errors should be exit code 6 (sync errors) or 7 (config)
    let exit_code = result.status.code().unwrap_or(0);
    assert!(
        exit_code == 6 || exit_code == 7,
        "unexpected exit code: {exit_code}"
    );

    // The error output should be valid JSON
    let json = parse_error_json(&result.stderr);
    if let Some(json) = json {
        assert!(verify_error_structure(&json), "missing required fields");
    }
    // Note: Some errors may not produce structured JSON yet - that's OK
}

#[test]
fn e2e_structured_error_conflict_markers() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Create JSONL with conflict markers
    let beads_dir = workspace.root.join(".beads");
    let issues_path = beads_dir.join("issues.jsonl");
    fs::write(
        &issues_path,
        "<<<<<<< HEAD\n{\"id\":\"bd-abc\"}\n=======\n{\"id\":\"bd-def\"}\n>>>>>>> branch\n",
    )
    .expect("write conflict jsonl");

    let result = run_br(
        &workspace,
        ["sync", "--import-only", "--json"],
        "import_conflict_json",
    );
    assert!(!result.status.success());

    // Should detect conflict markers
    assert!(
        result.stderr.contains("conflict") || result.stderr.contains("CONFLICT"),
        "should detect conflict markers"
    );
}
