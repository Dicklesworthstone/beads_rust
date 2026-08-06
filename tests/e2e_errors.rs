mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use std::fs;

fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    // Handle both formats: "Created bd-xxx: title" and "✓ Created bd-xxx: title"
    let normalized = line.strip_prefix("✓ ").unwrap_or(line);
    let id_part = normalized
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
    let _log = common::test_log("e2e_error_handling");
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

    let bad_blocked_priority = run_br(
        &workspace,
        ["blocked", "--priority", "9"],
        "blocked_bad_priority",
    );
    assert!(!bad_blocked_priority.status.success());

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
    let _log = common::test_log("e2e_dependency_errors");
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
    let _log = common::test_log("e2e_sync_invalid_orphans");
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
    let _log = common::test_log("e2e_sync_export_guards");
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
    // Use a complete Issue JSON (not just {"id":"bd-missing"}) to avoid parse errors during auto-import
    contents.push_str("{\"id\":\"bd-missing\",\"title\":\"Ghost issue\",\"status\":\"open\",\"priority\":2,\"issue_type\":\"task\",\"created_at\":\"2026-01-01T00:00:00Z\",\"updated_at\":\"2026-01-01T00:00:00Z\"}\n");
    fs::write(&issues_path, contents).expect("append jsonl");

    // Use --no-auto-import and --allow-stale to prevent bd-missing from being imported into DB
    let create2 = run_br(
        &workspace,
        ["create", "Dirty issue", "--no-auto-import", "--allow-stale"],
        "create_dirty",
    );
    assert!(
        create2.status.success(),
        "create failed: {}",
        create2.stderr
    );

    // The flush should fail because JSONL has bd-missing but DB doesn't
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
    let _log = common::test_log("e2e_ambiguous_id");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let mut ids: Vec<String> = Vec::new();
    let mut attempt = 0;
    let mut ambiguous_prefix: Option<String> = None;

    while ambiguous_prefix.is_none() && attempt < 30 {
        let title = format!("Ambiguous {attempt}");
        let create = run_br(&workspace, ["create", &title], "create_ambiguous");
        assert!(create.status.success(), "create failed: {}", create.stderr);
        let id = parse_created_id(&create.stdout);
        ids.push(id);

        // Check for first-character collisions (matches how the resolver
        // uses contains() -- a single char matches any hash containing it)
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

    let ambiguous_input = ambiguous_prefix.expect("failed to find ambiguous prefix");

    let show = run_br(&workspace, ["show", &ambiguous_input], "show_ambiguous");
    assert!(!show.status.success(), "ambiguous id should fail");
}

#[test]
fn e2e_lint_before_init_fails() {
    let _log = common::test_log("e2e_lint_before_init_fails");
    let workspace = BrWorkspace::new();
    let lint = run_br(&workspace, ["lint"], "lint_before_init");
    assert!(!lint.status.success());
}

#[test]
fn e2e_lint_clean_output_when_no_warnings() {
    let _log = common::test_log("e2e_lint_clean_output_when_no_warnings");
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
    let _log = common::test_log("e2e_lint_bug_missing_sections_json");
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
    let _log = common::test_log("e2e_lint_multiple_issues_aggregate_warnings");
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
    let _log = common::test_log("e2e_lint_text_output_exit_code");
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
    let _log = common::test_log("e2e_lint_status_all_includes_closed");
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
    let _log = common::test_log("e2e_lint_type_filter_limits_results");
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
    let _log = common::test_log("e2e_lint_ids_only_lints_selected");
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
    let _log = common::test_log("e2e_lint_skips_types_without_required_sections");
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

/// Parse the structured error envelope out of stderr.
///
/// **stderr is a mixed stream, not a JSON document, and never was one.** It
/// carries, in order: tracing output and human diagnostics (`warning: ct-1 is
/// already 'closed' ...`), the JSON envelope, and then whatever the command
/// logs on the way out — a trailing `DEBUG ... Auto-flush` line, and, since the
/// failure banner landed, `br: FAILED (CODE, exit N)`.
///
/// This helper used to try `from_str` on the whole stream and then on
/// everything from the first `{` to EOF. That is tolerant of *leading* noise
/// only — which is itself evidence that someone hit the leading case and
/// patched the reader rather than fixing the premise. Any trailing byte broke
/// it, and trailing bytes already occurred before the banner existed
/// (`br close <already-closed> --json` logs after the envelope; `jq .` on that
/// stderr exits 5 today).
///
/// So: find the envelope's opening brace and read exactly the *first* JSON
/// value from there, ignoring anything after it. This is the idiom
/// `tests/e2e_close_truth.rs::envelope` already uses; this helper simply
/// predates it.
fn parse_error_json(stderr: &str) -> Option<Value> {
    let start = stderr.find('{')?;
    serde_json::Deserializer::from_str(&stderr[start..])
        .into_iter()
        .next()?
        .ok()
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
    let _log = common::test_log("e2e_structured_error_not_initialized");
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
    let _log = common::test_log("e2e_structured_error_issue_not_found");
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
    let _log = common::test_log("e2e_structured_error_invalid_status");
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
    let _log = common::test_log("e2e_structured_error_cycle_detected");
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
    let _log = common::test_log("e2e_structured_error_self_dependency");
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
    let _log = common::test_log("e2e_structured_error_ambiguous_id");
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
    let _log = common::test_log("e2e_structured_error_jsonl_parse");
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
    let _log = common::test_log("e2e_structured_error_conflict_markers");
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

#[test]
fn e2e_custom_type_accepted() {
    let _log = common::test_log("e2e_custom_type_accepted");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Custom types are accepted (not rejected as invalid)
    let result = run_br(
        &workspace,
        ["create", "Test issue", "--type", "custom_type", "--json"],
        "create_custom_type_json",
    );
    assert!(
        result.status.success(),
        "custom types should be accepted: {}",
        result.stderr
    );

    // Verify the custom type is stored correctly
    let json: serde_json::Value =
        serde_json::from_str(&result.stdout).expect("should be valid JSON");
    assert_eq!(
        json["issue_type"], "custom_type",
        "custom type should be preserved"
    );
}

#[test]
fn e2e_structured_error_invalid_priority() {
    let _log = common::test_log("e2e_structured_error_invalid_priority");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Test invalid priority (out of 0-4 range)
    let result = run_br(
        &workspace,
        ["create", "Test issue", "--priority", "10", "--json"],
        "create_invalid_priority_json",
    );
    assert!(!result.status.success());
    assert_eq!(result.status.code(), Some(4), "exit code should be 4");

    let json = parse_error_json(&result.stderr).expect("should be valid JSON");
    assert!(verify_error_structure(&json), "missing required fields");

    let error = &json["error"];
    assert_eq!(error["code"], "INVALID_PRIORITY");
    assert!(error["retryable"].as_bool().unwrap());
    let hint = error["hint"].as_str().unwrap();
    assert!(
        hint.contains('0') && hint.contains('4') || hint.contains("between"),
        "hint should mention valid priority range, got: {hint}"
    );
}

// === --no-color mode tests for stable snapshots ===

#[test]
fn e2e_error_text_mode_no_color() {
    let _log = common::test_log("e2e_error_text_mode_no_color");
    let workspace = BrWorkspace::new();

    // Test NOT_INITIALIZED error in no-color mode
    let result = run_br(&workspace, ["list", "--no-color"], "list_not_init_no_color");
    assert!(!result.status.success());

    // Output should not contain ANSI escape codes
    assert!(
        !result.stderr.contains("\x1b["),
        "stderr should not contain ANSI escape codes"
    );
    assert!(
        !result.stdout.contains("\x1b["),
        "stdout should not contain ANSI escape codes"
    );
}

#[test]
fn e2e_error_text_vs_json_parity() {
    let _log = common::test_log("e2e_error_text_vs_json_parity");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Same error in text mode
    let text_result = run_br(
        &workspace,
        ["show", "bd-nonexistent", "--no-color"],
        "show_missing_text",
    );
    assert!(!text_result.status.success());

    // Same error in JSON mode
    let json_result = run_br(
        &workspace,
        ["show", "bd-nonexistent", "--json"],
        "show_missing_json",
    );
    assert!(!json_result.status.success());

    // Both should have same exit code
    assert_eq!(
        text_result.status.code(),
        json_result.status.code(),
        "text and JSON mode should have same exit code"
    );

    // JSON mode should produce valid structured error
    let json = parse_error_json(&json_result.stderr).expect("JSON mode should produce valid JSON");
    assert!(
        verify_error_structure(&json),
        "JSON error should have required fields"
    );

    // Text mode output should contain error message (not JSON)
    assert!(
        text_result.stderr.contains("not found") || text_result.stderr.contains("No issue"),
        "text mode should contain human-readable error"
    );
}

#[test]
fn e2e_error_multiple_errors_same_exit_code() {
    let _log = common::test_log("e2e_error_multiple_errors_same_exit_code");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success());
    let id = parse_created_id(&create.stdout);

    // Validation errors should return exit code 4
    // Note: invalid type is NOT tested here because custom types are allowed
    let invalid_status = run_br(
        &workspace,
        ["update", &id, "--status", "bad_status", "--json"],
        "invalid_status",
    );
    let invalid_priority = run_br(
        &workspace,
        ["create", "Test", "--priority", "99", "--json"],
        "invalid_priority",
    );

    assert_eq!(
        invalid_status.status.code(),
        Some(4),
        "invalid status should be exit 4"
    );
    assert_eq!(
        invalid_priority.status.code(),
        Some(4),
        "invalid priority should be exit 4"
    );
}

#[test]
fn e2e_error_exit_code_categories() {
    let _log = common::test_log("e2e_error_exit_code_categories");
    let workspace = BrWorkspace::new();

    // Exit code 2: Database/initialization errors
    let not_init = run_br(&workspace, ["list", "--json"], "not_init");
    assert_eq!(
        not_init.status.code(),
        Some(2),
        "NOT_INITIALIZED should be exit 2"
    );

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Exit code 3: Issue errors
    let not_found = run_br(&workspace, ["show", "bd-missing", "--json"], "not_found");
    assert_eq!(
        not_found.status.code(),
        Some(3),
        "ISSUE_NOT_FOUND should be exit 3"
    );

    // Exit code 4: Validation errors (already tested above)

    // Exit code 5: Dependency errors
    let create = run_br(&workspace, ["create", "Self dep"], "create_self");
    assert!(create.status.success());
    let id = parse_created_id(&create.stdout);

    let self_dep = run_br(&workspace, ["dep", "add", &id, &id, "--json"], "self_dep");
    assert_eq!(
        self_dep.status.code(),
        Some(5),
        "SELF_DEPENDENCY should be exit 5"
    );
}

// === Additional Validation + Error Parity Tests ===
//
// NOTE: `e2e_structured_error_label_validation` and
// `e2e_structured_error_label_too_long` were removed. Both exercised
// `update --add-label` to trigger a VALIDATION_FAILED error, but that flag
// was removed from the CLI (`#[arg(skip)]`, back-compat field only). The
// only surviving label-write surface, markdown bulk-import, treats invalid
// labels as a non-fatal warning-and-skip rather than a hard error (see
// `execute_import` in src/cli/commands/create.rs), so there is currently no
// reachable CLI path that produces a label VALIDATION_FAILED error at all.
// This is a real (if narrow) coverage gap worth flagging, not something to
// paper over by deleting silently.

#[test]
fn e2e_structured_error_dependency_target_not_found() {
    let _log = common::test_log("e2e_structured_error_dependency_target_not_found");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success());
    let id = parse_created_id(&create.stdout);

    // Try to add dependency on non-existent issue
    // The implementation returns ISSUE_NOT_FOUND for missing dependency targets
    let result = run_br(
        &workspace,
        ["dep", "add", &id, "bd-nonexistent", "--json"],
        "dep_missing_target_json",
    );
    assert!(!result.status.success());
    assert_eq!(
        result.status.code(),
        Some(3),
        "exit code should be 3 (issue not found)"
    );

    let json = parse_error_json(&result.stderr).expect("should be valid JSON");
    assert!(verify_error_structure(&json), "missing required fields");

    let error = &json["error"];
    // Returns ISSUE_NOT_FOUND since the target issue doesn't exist
    assert_eq!(error["code"], "ISSUE_NOT_FOUND");
    assert!(!error["retryable"].as_bool().unwrap());
    assert!(
        error["context"]["searched_id"]
            .as_str()
            .unwrap()
            .contains("nonexistent")
    );
}

#[test]
fn e2e_dependency_idempotent_duplicate() {
    let _log = common::test_log("e2e_dependency_idempotent_duplicate");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create_a = run_br(&workspace, ["create", "Issue A"], "create_a");
    assert!(create_a.status.success());
    let id_a = parse_created_id(&create_a.stdout);

    let create_b = run_br(&workspace, ["create", "Issue B"], "create_b");
    assert!(create_b.status.success());
    let id_b = parse_created_id(&create_b.stdout);

    // Add dependency first time - should succeed
    let dep_add = run_br(&workspace, ["dep", "add", &id_a, &id_b], "dep_add_first");
    assert!(dep_add.status.success());

    // Add same dependency again - should succeed (idempotent) with status "exists"
    let result = run_br(
        &workspace,
        ["dep", "add", &id_a, &id_b, "--json"],
        "dep_add_duplicate_json",
    );
    assert!(
        result.status.success(),
        "duplicate dependency should be idempotent"
    );

    // Parse output as success JSON (not error)
    let json: Value = serde_json::from_str(&result.stdout).expect("should be valid JSON");
    assert_eq!(
        json["status"].as_str().unwrap_or(""),
        "exists",
        "status should be 'exists'"
    );
    assert_eq!(
        json["action"].as_str().unwrap_or(""),
        "already_exists",
        "action should be 'already_exists'"
    );
}

#[test]
fn e2e_delete_with_dependents_preview() {
    let _log = common::test_log("e2e_delete_with_dependents_preview");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create_a = run_br(&workspace, ["create", "Issue A"], "create_a");
    assert!(create_a.status.success());
    let id_a = parse_created_id(&create_a.stdout);

    let create_b = run_br(&workspace, ["create", "Issue B"], "create_b");
    assert!(create_b.status.success());
    let id_b = parse_created_id(&create_b.stdout);

    // B depends on A
    let dep_add = run_br(&workspace, ["dep", "add", &id_b, &id_a], "dep_add");
    assert!(dep_add.status.success());

    // Delete A (which has B as dependent) - shows preview mode warning
    // The command exits 0 (preview mode) but warns about dependents
    let result = run_br(&workspace, ["delete", &id_a], "delete_with_deps");
    assert!(
        result.status.success(),
        "delete with dependents should show preview"
    );
    assert!(
        result.stdout.contains("depend on") || result.stdout.contains("dependents"),
        "should mention dependents in output"
    );
    assert!(
        result.stdout.contains("--force") || result.stdout.contains("--cascade"),
        "should suggest force or cascade options"
    );

    // Issue should still exist after preview
    let show = run_br(&workspace, ["show", &id_a], "show_after_preview");
    assert!(
        show.status.success(),
        "issue should still exist after preview"
    );
}

// NOTE: `e2e_validation_error_empty_label` and
// `e2e_validation_special_characters_in_label` were removed for the same
// reason as above: both exercised `update --add-label`, which no longer
// exists as a CLI surface, and the only surviving label-write path
// (markdown bulk-import) doesn't hard-fail on invalid labels.

#[test]
fn e2e_error_text_json_parity_validation() {
    // NOTE: originally used `update --add-label` to trigger a validation
    // error, but that flag was removed from the CLI (labels can now only
    // be set via markdown bulk-import, which treats invalid labels as a
    // non-fatal warning rather than a hard error, so it can't reach this
    // path either). Ported to use an invalid `--status` value instead,
    // which still exercises the same text/JSON error-parity behavior this
    // test is actually about.
    let _log = common::test_log("e2e_error_text_json_parity_validation");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success());
    let id = parse_created_id(&create.stdout);

    // Same validation error in text mode
    let text_result = run_br(
        &workspace,
        ["update", &id, "--status", "done", "--no-color"],
        "status_error_text",
    );
    assert!(!text_result.status.success());

    // Same validation error in JSON mode
    let json_result = run_br(
        &workspace,
        ["update", &id, "--status", "done", "--json"],
        "status_error_json",
    );
    assert!(!json_result.status.success());

    // Both should have same exit code
    assert_eq!(
        text_result.status.code(),
        json_result.status.code(),
        "text and JSON mode should have same exit code for validation errors"
    );

    // JSON mode should produce valid structured error
    let json = parse_error_json(&json_result.stderr).expect("JSON mode should produce valid JSON");
    assert!(
        verify_error_structure(&json),
        "JSON error should have required fields"
    );
}

// =============================================================================
// The stderr contract: a MIXED STREAM, and never a single JSON document
// =============================================================================

/// stderr carries diagnostics, then the envelope, then trailing output — and it
/// did so *before* the failure banner existed.
///
/// This is the premise the documented recipe in `docs/agent/ERRORS.md` used to
/// rest on (`br ... 2>err.json; jq . err.json`). It has been false for as long
/// as `close` has printed `warning:` lines: `jq .` on that stderr exits 5.
/// `parse_error_json` was written tolerant of *leading* noise only, which is the
/// fossil of someone hitting the first half of this and patching the reader
/// instead of the premise.
///
/// Three shapes, three different generators, pinned separately because a fix
/// for one does not imply the others:
///
/// - **(a) the negative control**: envelope alone, no leading noise. It passes
///   with any reader, including a broken one, and it is the case a natural test
///   would have chosen — which is exactly why this survived.
/// - **(b) one warning, then the envelope.**
/// - **(c) two warnings, then the envelope.** One invocation can emit the
///   contention warning more than once.
///
/// All three carry the banner as a trailing line, so each also pins the other
/// end of the stream.
///
/// Why this is more than tidiness: the warning generator is a *contention*
/// warning ("another agent may be working on this issue"). The envelope becomes
/// unparseable precisely on the concurrency paths — two agents colliding on one
/// bead — which is when the exact error detail matters most and when nobody is
/// watching the scrollback. The parse breaks in inverse proportion to how much
/// you need it.
#[test]
fn envelope_is_readable_under_every_leading_noise_shape() {
    struct Shape {
        name: &'static str,
        args: Vec<String>,
        warnings: usize,
        code: &'static str,
        exit: i32,
    }

    let _log = common::test_log("envelope_is_readable_under_every_leading_noise_shape");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init: {}", init.stderr);

    // A blocked issue gives a nonzero close, and an already-closed issue gives
    // the contention warning. Repeating the closed id repeats the warning.
    let blocker = create_issue(&workspace, "blocker", "noise_blocker");
    let blocked = create_issue(&workspace, "blocked", "noise_blocked");
    let dep = run_br(
        &workspace,
        ["dep", "add", &blocked, &blocker],
        "noise_dep_add",
    );
    assert!(dep.status.success(), "dep add: {}", dep.stderr);

    let closed = create_issue(&workspace, "already closed", "noise_closed");
    let first = run_br(&workspace, ["close", &closed], "noise_close_first");
    assert!(first.status.success(), "first close: {}", first.stderr);

    let shapes = vec![
        Shape {
            name: "(a) negative control: envelope alone, no leading noise",
            args: vec!["show".into(), "bd-nonexistent".into(), "--json".into()],
            warnings: 0,
            code: "ISSUE_NOT_FOUND",
            exit: 3,
        },
        Shape {
            name: "(b) one warning, then the envelope",
            args: vec![
                "close".into(),
                closed.clone(),
                blocked.clone(),
                "--json".into(),
            ],
            warnings: 1,
            code: "NOTHING_TO_DO",
            exit: 3,
        },
        Shape {
            name: "(c) two warnings, then the envelope",
            args: vec![
                "close".into(),
                closed.clone(),
                closed.clone(),
                blocked.clone(),
                "--json".into(),
            ],
            warnings: 2,
            code: "NOTHING_TO_DO",
            exit: 3,
        },
    ];

    for shape in shapes {
        let label = format!("noise_shape_{}", shape.warnings);
        let result = run_br(&workspace, &shape.args, &label);
        let stderr = &result.stderr;
        let what = shape.name;

        assert_eq!(
            result.status.code(),
            Some(shape.exit),
            "{what}: exit code: {stderr}"
        );

        let warnings = stderr
            .lines()
            .filter(|line| {
                let lower = line.to_ascii_lowercase();
                lower.starts_with("warning:")
            })
            .count();
        assert_eq!(
            warnings, shape.warnings,
            "{what}: expected {} warning line(s); if a generator changed, this \
             test's premise moved and the counts need re-deriving: {stderr}",
            shape.warnings
        );

        // The reader must scan to the first '{'. Nothing weaker works: dropping
        // a fixed number of leading lines is defeated by shape (c), and
        // matching a case-sensitive `warning:` prefix is defeated by the
        // capital-W generator (see the fixture test below).
        let json = parse_error_json(stderr)
            .unwrap_or_else(|| panic!("{what}: envelope must be extractable: {stderr}"));
        assert_eq!(json["error"]["code"], shape.code, "{what}: envelope code");
        assert!(
            verify_error_structure(&json),
            "{what}: envelope shape: {json}"
        );

        // And the banner is the last line of that stream, whatever preceded it.
        let last = stderr.lines().next_back().unwrap_or_default();
        assert!(
            last.contains(&format!("FAILED ({}, exit {})", shape.code, shape.exit)),
            "{what}: banner must be last: {stderr}"
        );

        // Shape (a) is the control: it is the one a broken reader also passes.
        if shape.warnings == 0 {
            assert!(
                stderr.starts_with('{'),
                "{what}: the control must have no leading noise, otherwise it \
                 is not controlling for anything: {stderr}"
            );
        }
    }
}

/// The reader itself, against every noise shape the tree can produce — including
/// **mixed capitalisation**, which no CLI path can currently put in front of a
/// JSON error envelope but which the tree is one refactor away from producing.
///
/// There are two warning generators with different spellings:
/// `src/cli/commands/update.rs` writes lowercase `warning:` unconditionally,
/// while `src/output/context.rs` writes capital `Warning:` and is suppressed in
/// JSON mode. So a reader that matched `^warning:` case-sensitively would pass
/// every end-to-end test above and still break the day the other generator
/// reaches a JSON error path. Fixtures, not invocations, are the honest way to
/// pin that: this test says what the reader must tolerate rather than what the
/// binary happens to emit today.
#[test]
fn envelope_reader_tolerates_mixed_capitalisation_and_trailing_noise() {
    let _log = common::test_log("envelope_reader_tolerates_mixed_capitalisation_and_trailing_noise");
    let envelope = "{\n  \"error\": {\n    \"code\": \"NOTHING_TO_DO\",\n    \
                    \"message\": \"m\",\n    \"retryable\": false\n  }\n}\n";
    let lower = "warning: ct-1 is already 'closed' (set 50m ago by 'toad') — \
                 another agent may be working on this issue\n";
    let upper = "Warning: invalid label 'a b': labels may not contain spaces\n";
    let log = "2026-08-06T16:41:14.666352Z  INFO beads_rust::cli::commands::close: \
               src/cli/commands/close.rs:151: Executing close command\n";
    let trailing_log = "2026-08-06T16:41:14.676425Z DEBUG beads_rust::sync: \
                        src/sync/mod.rs:1853: Auto-flush: no dirty issues, skipping\n";
    let banner = "br: FAILED (NOTHING_TO_DO, exit 3)\n";

    let leading: Vec<(&str, String)> = vec![
        ("nothing", String::new()),
        ("a log line", log.to_string()),
        ("one lowercase warning", lower.to_string()),
        ("two lowercase warnings", format!("{lower}{lower}")),
        ("lowercase then capital", format!("{lower}{upper}")),
        ("capital then lowercase", format!("{upper}{lower}")),
        ("logs and both spellings", format!("{log}{upper}{lower}")),
    ];
    let trailing: Vec<(&str, String)> = vec![
        ("nothing", String::new()),
        ("the banner", banner.to_string()),
        ("a log line", trailing_log.to_string()),
        ("a log line then the banner", format!("{trailing_log}{banner}")),
    ];

    for (before_name, before) in &leading {
        for (after_name, after) in &trailing {
            let stderr = format!("{before}{envelope}{after}");
            let json = parse_error_json(&stderr).unwrap_or_else(|| {
                panic!("reader failed with {before_name} before and {after_name} after: {stderr}")
            });
            assert_eq!(
                json["error"]["code"], "NOTHING_TO_DO",
                "wrong value extracted with {before_name} before and {after_name} after"
            );
        }
    }
}

/// The trailing-noise half of the contract, demonstrated with NO banner in the
/// picture at all — this exit is 0.
///
/// `br close <already-closed> --json` prints a `warning:` line, then a
/// `notice` envelope, then a `DEBUG ... Auto-flush` line from the sync layer on
/// the way out. That trailing line has been there far longer than the failure
/// banner, and it is why `jq . err.json` — the recipe `docs/agent/ERRORS.md`
/// used to document — exits 5 on a perfectly ordinary command.
///
/// So this test earns the `parse_error_json` fix independently of the banner:
/// revert the helper to its leading-noise-only form and it fails here even on a
/// binary built before the banner existed (verified both ways).
#[test]
fn envelope_extraction_survives_trailing_logs_with_no_banner() {
    let _log = common::test_log("envelope_extraction_survives_trailing_logs_with_no_banner");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init: {}", init.stderr);

    let id = create_issue(&workspace, "already closed thing", "trailing_create");
    let first = run_br(&workspace, ["close", &id], "trailing_close_first");
    assert!(first.status.success(), "first close: {}", first.stderr);

    let result = run_br(&workspace, ["close", &id, "--json"], "trailing_close_again");
    assert_eq!(
        result.status.code(),
        Some(0),
        "closing an already-closed issue is not a failure: {}",
        result.stderr
    );

    let stderr = &result.stderr;

    // No banner: this exit is 0. Whatever trails the envelope here is not ours.
    assert!(
        !stderr.contains("FAILED ("),
        "exit 0 must not carry a failure banner: {stderr}"
    );

    // Something trails the envelope anyway.
    assert!(
        !stderr.trim_end().ends_with('}'),
        "expected the sync layer to log after the envelope; if this ever stops \
         being true, trailing-tolerance in parse_error_json is still required \
         by the banner, but this test no longer proves it independently: \
         {stderr}"
    );

    // The documented recipe's premise, false without any help from us.
    assert!(
        serde_json::from_str::<Value>(stderr).is_err(),
        "stderr parsed as a single JSON document, which docs/agent/ERRORS.md \
         used to assume: {stderr}"
    );

    // And the envelope is extractable regardless.
    let json = parse_error_json(stderr).expect("envelope must survive the trailing logs");
    assert_eq!(json["notice"]["code"], "ALREADY_SATISFIED");
}

/// Create an issue and return its id.
fn create_issue(workspace: &BrWorkspace, title: &str, label: &str) -> String {
    let out = run_br(workspace, ["create", title, "--json"], label);
    assert!(out.status.success(), "create failed: {}", out.stderr);
    let payload = extract_json_payload(&out.stdout);
    let json: Value = serde_json::from_str(&payload).expect("create json");
    json["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no id in create output: {json}"))
        .to_string()
}
