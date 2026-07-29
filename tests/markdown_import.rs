mod common;
use common::cli::{BrWorkspace, extract_json_payload, parse_list_issues, run_br};
use serde_json::Value;
use std::fs;

#[test]
fn test_markdown_import() {
    let workspace = BrWorkspace::new();

    // Initialize
    let output = run_br(&workspace, ["init"], "init");
    assert!(output.status.success(), "init failed");

    // Create markdown file
    let md_path = workspace.root.join("issues.md");
    // We use content_safe below. The logic validation of dependencies is commented out
    // because we can't easily refer to new issue IDs in markdown import without placeholders.

    let content_safe = r"## First Issue
### Priority
1
### Labels
bug, frontend

## Second Issue
Implicit description here.

### Type
feature
";

    fs::write(&md_path, content_safe).expect("write md");

    // Run create --file
    let output = run_br(&workspace, ["create", "--file", "issues.md"], "create_md");
    println!("stdout:\n{}", output.stdout);
    println!("stderr:\n{}", output.stderr);
    assert!(output.status.success(), "create --file failed");

    assert!(output.stdout.contains("✓ Created 2 issues from issues.md:"));
    assert!(
        output
            .stdout
            .lines()
            .any(|line| line.starts_with("  ") && line.contains(": First Issue")),
        "expected indented created-issue line in stdout: {}",
        output.stdout
    );

    // Verify list
    let output = run_br(&workspace, ["list"], "list");
    assert!(output.status.success());
    assert!(output.stdout.contains("First Issue"));
    assert!(output.stdout.contains("Second Issue"));
    assert!(output.stdout.contains("P1]")); // Priority 1 (format: [● P1])

    // Verify labels on First Issue using JSON output.
    //
    // beads_rust-44rc rewrite (2026-05-09): originally pinned the
    // pretty-printed JSON format `"title": "First Issue"` (with a space
    // after `:`). After commit `f26bf73f fix(output): fail on stdout
    // serialization errors` and the streaming-perf migration in
    // `src/output/context.rs::json` (`serde_json::to_writer`, compact
    // format), the JSON has no whitespace between key and value. Switched
    // to semantic JSON parse + invariant checks so the test is robust to
    // format changes.
    let output = run_br(&workspace, ["list", "--json"], "list_json");
    assert!(output.status.success());

    let payload: Value = serde_json::from_str(output.stdout.trim())
        .expect("br list --json output must be valid JSON");
    let issues = payload
        .get("issues")
        .and_then(Value::as_array)
        .expect("expected `issues` array in br list --json output");

    let first = issues
        .iter()
        .find(|issue| issue.get("title").and_then(Value::as_str) == Some("First Issue"))
        .expect("expected an issue with title \"First Issue\" in br list --json output");

    let labels: Vec<&str> = first
        .get("labels")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    assert!(
        labels.contains(&"bug"),
        "expected `bug` label on First Issue; got labels={labels:?}"
    );
    assert!(
        labels.contains(&"frontend"),
        "expected `frontend` label on First Issue; got labels={labels:?}"
    );
}

#[test]
fn test_markdown_import_json_output() {
    let workspace = BrWorkspace::new();

    let output = run_br(&workspace, ["init"], "init_json");
    assert!(output.status.success(), "init failed");

    let md_path = workspace.root.join("issues.md");
    let content = r"## One
### Type
task

## Two
### Type
bug
";
    fs::write(&md_path, content).expect("write md");

    let output = run_br(
        &workspace,
        ["create", "--file", "issues.md", "--json"],
        "create_json",
    );
    assert!(output.status.success(), "create --file --json failed");

    let payload = extract_json_payload(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&payload).expect("json parse");
    let array = json.as_array().expect("json array");
    assert_eq!(array.len(), 2);
    assert!(payload.contains("\"One\""));
    assert!(payload.contains("\"Two\""));
}

/// beads_rust#304: `### Agent Context` maps to the issue's `agent_context`
/// field and surfaces in `--json` output when set; issues without the
/// section omit the field.
#[test]
fn test_markdown_import_agent_context_json() {
    let workspace = BrWorkspace::new();

    let output = run_br(&workspace, ["init"], "init_agent_ctx");
    assert!(output.status.success(), "init failed");

    let md_path = workspace.root.join("issues.md");
    let content = r#"## With Context
### Type
epic
### Agent Context
{"skills": ["porting-to-rust"], "constraints": ["no rusqlite; fsqlite only"]}

## Without Context
### Type
task
"#;
    fs::write(&md_path, content).expect("write md");

    let output = run_br(
        &workspace,
        ["create", "--file", "issues.md", "--json"],
        "create_agent_ctx_json",
    );
    assert!(
        output.status.success(),
        "create --file --json failed: {}",
        output.stderr
    );

    let payload = extract_json_payload(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&payload).expect("json parse");
    let array = json.as_array().expect("json array");
    assert_eq!(array.len(), 2);

    let with_ctx = array
        .iter()
        .find(|issue| issue.get("title").and_then(Value::as_str) == Some("With Context"))
        .expect("expected an issue titled \"With Context\"");
    let agent_context = with_ctx
        .get("agent_context")
        .and_then(Value::as_str)
        .expect("agent_context must be present in JSON when set");
    assert!(
        agent_context.contains("porting-to-rust"),
        "agent_context content not preserved: {agent_context}"
    );
    assert!(
        agent_context.contains("no rusqlite; fsqlite only"),
        "agent_context content not preserved: {agent_context}"
    );

    let without_ctx = array
        .iter()
        .find(|issue| issue.get("title").and_then(Value::as_str) == Some("Without Context"))
        .expect("expected an issue titled \"Without Context\"");
    assert!(
        without_ctx.get("agent_context").is_none(),
        "agent_context must be omitted from JSON when unset, got: {without_ctx}"
    );

    // Confirm persistence: `br show --json` echoes the stored agent_context.
    let with_id = with_ctx
        .get("id")
        .and_then(Value::as_str)
        .expect("created issue must have an id");
    let show = run_br(&workspace, ["show", with_id, "--json"], "show_agent_ctx");
    assert!(show.status.success(), "show failed: {}", show.stderr);
    assert!(
        show.stdout.contains("porting-to-rust"),
        "stored agent_context should round-trip through show --json: {}",
        show.stdout
    );
}

#[test]
fn test_markdown_import_updates_last_touched_context() {
    let workspace = BrWorkspace::new();

    let output = run_br(&workspace, ["init"], "init_last_touched_import");
    assert!(output.status.success(), "init failed");

    let md_path = workspace.root.join("issues.md");
    let content = r"## First imported
### Type
task

## Second imported
### Type
bug
";
    fs::write(&md_path, content).expect("write md");

    let output = run_br(
        &workspace,
        ["create", "--file", "issues.md"],
        "create_import_last_touched",
    );
    assert!(
        output.status.success(),
        "create --file failed: {}",
        output.stderr
    );

    let update = run_br(
        &workspace,
        ["update", "--status", "in_progress"],
        "update_after_import_last_touched",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);

    let list = run_br(
        &workspace,
        ["list", "--json"],
        "list_after_import_last_touched",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: serde_json::Value = serde_json::from_str(&payload).expect("json parse");
    // Handle both bare array and paginated {"issues": [...]} formats
    let issues = if let Some(arr) = json.as_array() {
        arr.clone()
    } else {
        json["issues"]
            .as_array()
            .expect("json issues array")
            .clone()
    };
    let second = issues
        .iter()
        .find(|issue| issue["title"] == "Second imported")
        .expect("second imported issue");
    assert_eq!(second["status"], "in_progress");
}

#[test]
fn test_markdown_import_implicit_description_keeps_first_non_empty_line_only() {
    let workspace = BrWorkspace::new();

    let output = run_br(&workspace, ["init"], "init_implicit_description");
    assert!(output.status.success(), "init failed");

    let md_path = workspace.root.join("issues.md");
    let content = r"## Implicit Description Issue
First line becomes description
This line should be ignored

### Type
task
";
    fs::write(&md_path, content).expect("write md");

    let output = run_br(
        &workspace,
        ["create", "--file", "issues.md", "--json"],
        "create_implicit_description_json",
    );
    assert!(output.status.success(), "create --file --json failed");

    let payload = extract_json_payload(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&payload).expect("json parse");
    let issues = json.as_array().expect("json array");
    assert_eq!(issues.len(), 1);
    assert_eq!(
        issues[0]["description"].as_str(),
        Some("First line becomes description")
    );
}

// `br create --dry-run --file <md>` validates the bulk import file and reports
// what would be created without persisting to storage or JSONL. (#300)
#[test]
fn test_markdown_import_dry_run_validates_without_persisting() {
    let workspace = BrWorkspace::new();

    let output = run_br(&workspace, ["init"], "init_dry_run");
    assert!(output.status.success(), "init failed");

    let md_path = workspace.root.join("issues.md");
    let content = r"## DryRun Issue
### Type
task

## Second DryRun Issue
### Type
bug
";
    fs::write(&md_path, content).expect("write md");

    let output = run_br(
        &workspace,
        ["create", "--file", "issues.md", "--dry-run"],
        "create_dry_run",
    );
    assert!(
        output.status.success(),
        "dry-run with --file should succeed; stderr={}",
        output.stderr
    );
    assert!(
        output.stdout.contains("Dry run: would create 2 issues"),
        "expected dry-run summary in stdout, got: {}",
        output.stdout
    );
    assert!(output.stdout.contains("DryRun Issue"));
    assert!(output.stdout.contains("Second DryRun Issue"));

    // Storage side-effects: nothing should be persisted.
    let list = run_br(&workspace, ["list"], "list_after_dry_run");
    assert!(list.status.success());
    assert!(
        !list.stdout.contains("DryRun Issue"),
        "dry-run must not write issues to storage; list output: {}",
        list.stdout
    );
}

// Dry-run JSON output must still emit the would-create payload so callers can
// pipe it into validation tools — even though nothing is persisted. (#300)
#[test]
fn test_markdown_import_dry_run_emits_json() {
    let workspace = BrWorkspace::new();

    let output = run_br(&workspace, ["init"], "init_dry_run_json");
    assert!(output.status.success(), "init failed");

    let md_path = workspace.root.join("issues.md");
    let content = r"## JsonDryRun Issue
### Type
task
";
    fs::write(&md_path, content).expect("write md");

    let output = run_br(
        &workspace,
        ["create", "--file", "issues.md", "--dry-run", "--json"],
        "create_dry_run_json",
    );
    assert!(
        output.status.success(),
        "dry-run --json with --file should succeed; stderr={}",
        output.stderr
    );
    let payload = extract_json_payload(&output.stdout);
    let parsed: Value =
        serde_json::from_str(&payload).expect("dry-run --json must emit valid JSON");
    let issues = parsed
        .as_array()
        .or_else(|| parsed.get("issues").and_then(Value::as_array))
        .expect("expected an array of would-create issues");
    assert_eq!(issues.len(), 1, "expected one would-create issue");
    assert_eq!(
        issues[0].get("title").and_then(Value::as_str),
        Some("JsonDryRun Issue")
    );
}

#[test]
fn test_markdown_import_rejects_title_argument() {
    let workspace = BrWorkspace::new();

    let output = run_br(&workspace, ["init"], "init_title_arg");
    assert!(output.status.success(), "init failed");

    let md_path = workspace.root.join("issues.md");
    let content = r"## Bulk Issue
### Type
task
";
    fs::write(&md_path, content).expect("write md");

    let output = run_br(
        &workspace,
        ["create", "SingleTitle", "--file", "issues.md"],
        "create_title_arg",
    );
    assert!(
        !output.status.success(),
        "title argument should fail with --file"
    );
    assert!(
        output
            .stderr
            .contains("cannot be combined with title arguments")
    );
}

#[test]
fn test_markdown_import_parent_argument_sets_global_default() {
    let workspace = BrWorkspace::new();

    let output = run_br(&workspace, ["init"], "init_parent_arg");
    assert!(output.status.success(), "init failed");

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
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("")
        .trim()
        .to_string();
    assert!(!parent_id.is_empty(), "expected parent issue id");

    let md_path = workspace.root.join("issues.md");
    let content = r"## Child from import
### Type
task
";
    fs::write(&md_path, content).expect("write md");

    // --parent with --file sets a global default parent for imported issues
    let output = run_br(
        &workspace,
        [
            "create",
            "--file",
            "issues.md",
            "--parent",
            &parent_id,
            "--json",
        ],
        "create_parent_arg",
    );
    assert!(
        output.status.success(),
        "--parent with --file should work: {}",
        output.stderr
    );

    let payload = extract_json_payload(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&payload).expect("json parse");
    let issues = json.as_array().expect("json array");
    assert_eq!(issues.len(), 1);

    // Verify the imported issue has a parent-child dependency on the parent
    let deps = issues[0]["dependencies"]
        .as_array()
        .expect("dependencies array");
    assert!(
        deps.iter().any(|d| {
            d["depends_on_id"].as_str() == Some(parent_id.as_str())
                && d["type"].as_str() == Some("parent-child")
        }),
        "imported issue should have parent-child dep on {parent_id}, got: {deps:?}"
    );
}

#[test]
fn test_markdown_import_unresolved_item_parent_skips_only_that_issue() {
    let workspace = BrWorkspace::new();

    let output = run_br(&workspace, ["init"], "init_unresolved_item_parent");
    assert!(output.status.success(), "init failed");

    let md_path = workspace.root.join("issues.md");
    let content = r"## Child with missing parent
### Parent
does-not-exist

## Independent import
### Type
task
";
    fs::write(&md_path, content).expect("write md");

    let output = run_br(
        &workspace,
        ["create", "--file", "issues.md", "--json"],
        "create_unresolved_item_parent",
    );
    assert!(
        output.status.success(),
        "one bad item parent should not abort the import: {}",
        output.stderr
    );
    assert!(
        output
            .stderr
            .contains("Failed to resolve parent for Child with missing parent"),
        "stderr should explain skipped parent resolution: {}",
        output.stderr
    );

    let payload = extract_json_payload(&output.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("json parse");
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0]["title"].as_str(), Some("Independent import"));
}

#[test]
fn test_markdown_import_rejects_external_ref_argument() {
    let workspace = BrWorkspace::new();

    let output = run_br(&workspace, ["init"], "init_external_ref_arg");
    assert!(output.status.success(), "init failed");

    let md_path = workspace.root.join("issues.md");
    let content = r"## Imported issue
### Type
task
";
    fs::write(&md_path, content).expect("write md");

    let output = run_br(
        &workspace,
        [
            "create",
            "--file",
            "issues.md",
            "--external-ref",
            "JIRA-123",
        ],
        "create_external_ref_arg",
    );
    assert!(
        !output.status.success(),
        "--external-ref should fail with --file"
    );
    assert!(
        output
            .stderr
            .contains("--external-ref is not supported with --file")
    );
}

#[test]
fn test_markdown_import_rejects_non_empty_file_without_issue_headers() {
    let workspace = BrWorkspace::new();

    let output = run_br(&workspace, ["init"], "init_no_headers");
    assert!(output.status.success(), "init failed");

    let md_path = workspace.root.join("issues.md");
    let content = r"### Description
This file has content but no issue headers.
";
    fs::write(&md_path, content).expect("write md");

    let output = run_br(
        &workspace,
        ["create", "--file", "issues.md"],
        "create_no_headers",
    );
    assert!(
        !output.status.success(),
        "non-empty file without issue headers should fail"
    );
    assert!(
        output
            .stderr
            .contains("no issues found; expected '## Title' headers")
    );
}

#[test]
fn test_markdown_import_dependency_bullets_do_not_create_marker_dependency() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_bullet_deps");
    assert!(init.status.success(), "init failed");

    let blocker = run_br(
        &workspace,
        ["create", "Blocker for markdown import", "--json"],
        "create_blocker_json",
    );
    assert!(
        blocker.status.success(),
        "create blocker failed: {}",
        blocker.stderr
    );
    let blocker_payload = extract_json_payload(&blocker.stdout);
    let blocker_json: serde_json::Value =
        serde_json::from_str(&blocker_payload).expect("blocker json");
    let blocker_id = blocker_json["id"].as_str().expect("blocker id").to_string();

    let md_path = workspace.root.join("issues.md");
    let content =
        format!("## Imported issue\n### Dependencies\n- {blocker_id}\n- [ ] external:github#123\n");
    fs::write(&md_path, content).expect("write md");

    let output = run_br(
        &workspace,
        ["create", "--file", "issues.md", "--json"],
        "create_bullet_deps_json",
    );
    assert!(
        output.status.success(),
        "create --file --json failed: {}",
        output.stderr
    );

    let payload = extract_json_payload(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&payload).expect("json parse");
    let issues = json.as_array().expect("json array");
    assert_eq!(issues.len(), 1);

    let dependencies = issues[0]["dependencies"]
        .as_array()
        .expect("dependencies array");
    assert_eq!(dependencies.len(), 2);
    assert!(
        dependencies
            .iter()
            .any(|dep| dep["depends_on_id"].as_str() == Some(blocker_id.as_str()))
    );
    assert!(
        dependencies
            .iter()
            .any(|dep| dep["depends_on_id"].as_str() == Some("external:github#123"))
    );
    assert!(
        dependencies
            .iter()
            .all(|dep| dep["depends_on_id"].as_str() != Some("-"))
    );
}

#[test]
fn test_markdown_import_invalid_dependency_warns() {
    let workspace = BrWorkspace::new();

    let output = run_br(&workspace, ["init"], "init_invalid_dep");
    assert!(output.status.success(), "init failed");

    let md_path = workspace.root.join("issues.md");
    let content = r"## Issue With Bad Dep
### Dependencies
invalid-type:bd-123
";
    fs::write(&md_path, content).expect("write md");

    let output = run_br(
        &workspace,
        ["create", "--file", "issues.md"],
        "create_bad_dep",
    );
    assert!(
        output.status.success(),
        "create should succeed with warnings"
    );
    assert!(
        output
            .stderr
            .contains("Issue not found: invalid-type:bd-123"),
        "expected warning for missing issue id"
    );
}

#[test]
fn test_markdown_import_all_failed_returns_error() {
    let workspace = BrWorkspace::new();

    let output = run_br(&workspace, ["init"], "init_all_failed");
    assert!(output.status.success(), "init failed");

    let md_path = workspace.root.join("issues.md");
    let content = r"## Broken One
### Priority
999

## Broken Two
### Priority
999
";
    fs::write(&md_path, content).expect("write md");

    let output = run_br(
        &workspace,
        ["create", "--file", "issues.md", "--json"],
        "create_all_failed",
    );
    assert!(
        !output.status.success(),
        "all-failed markdown import should return an error"
    );
    assert!(
        output.stderr.contains("failed to create any issues from"),
        "expected summary failure, got: {}",
        output.stderr
    );

    let list = run_br(
        &workspace,
        ["list", "--json"],
        "list_after_all_failed_import",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);
    let listed = parse_list_issues(&list.stdout);
    assert_eq!(listed.len(), 0);
}

#[test]
fn test_markdown_import_whitespace_separated_typed_dependencies() {
    let workspace = BrWorkspace::new();

    let output = run_br(&workspace, ["init"], "init_whitespace_typed_deps");
    assert!(output.status.success(), "init failed");

    let blocker = run_br(
        &workspace,
        ["create", "Whitespace dependency blocker", "--json"],
        "create_whitespace_dep_blocker_json",
    );
    assert!(
        blocker.status.success(),
        "create blocker failed: {}",
        blocker.stderr
    );
    let blocker_payload = extract_json_payload(&blocker.stdout);
    let blocker_json: serde_json::Value =
        serde_json::from_str(&blocker_payload).expect("blocker json");
    let blocker_id = blocker_json["id"].as_str().expect("blocker id").to_string();

    let md_path = workspace.root.join("issues.md");
    let content =
        format!("## Imported issue\n### Dependencies\nblocks: {blocker_id} external:github#123\n");
    fs::write(&md_path, content).expect("write md");

    let output = run_br(
        &workspace,
        ["create", "--file", "issues.md", "--json"],
        "create_whitespace_typed_deps_json",
    );
    assert!(
        output.status.success(),
        "create --file --json failed: {}",
        output.stderr
    );

    let payload = extract_json_payload(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&payload).expect("json parse");
    let issues = json.as_array().expect("json array");
    assert_eq!(issues.len(), 1);

    let dependencies = issues[0]["dependencies"]
        .as_array()
        .expect("dependencies array");
    assert_eq!(dependencies.len(), 2);
    assert!(
        dependencies
            .iter()
            .any(|dep| dep["depends_on_id"].as_str() == Some(blocker_id.as_str()))
    );
    assert!(
        dependencies
            .iter()
            .any(|dep| dep["depends_on_id"].as_str() == Some("external:github#123"))
    );
}

#[test]
fn test_markdown_import_standin_id_dependency_resolution() {
    let workspace = BrWorkspace::new();

    let output = run_br(&workspace, ["init"], "init_standin");
    assert!(output.status.success(), "init failed");

    // Create a markdown file where issues reference each other via stand-in IDs
    let md_path = workspace.root.join("issues.md");
    let content = r"## Build Database Schema
### ID
db-1
### Type
task
### Priority
0

## Build API Endpoints
### Type
feature
### Dependencies
- db-1
";
    fs::write(&md_path, content).expect("write md");

    let output = run_br(
        &workspace,
        ["create", "--file", "issues.md", "--json"],
        "create_standin_deps_json",
    );
    assert!(
        output.status.success(),
        "create --file --json failed: {}",
        output.stderr
    );

    let payload = extract_json_payload(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&payload).expect("json parse");
    let issues = json.as_array().expect("json array");
    assert_eq!(issues.len(), 2);

    // The second issue (API Endpoints) should depend on the first (Database Schema)
    let db_id = issues[0]["id"].as_str().expect("db issue id");
    let api_deps = issues[1]["dependencies"]
        .as_array()
        .expect("api dependencies array");
    assert_eq!(
        api_deps.len(),
        1,
        "expected 1 dependency, got {}: {api_deps:?}",
        api_deps.len()
    );
    assert_eq!(
        api_deps[0]["depends_on_id"].as_str(),
        Some(db_id),
        "dependency should resolve stand-in 'db-1' to the generated ID of Database Schema"
    );
}

#[test]
fn test_markdown_import_title_based_dependency_resolution() {
    let workspace = BrWorkspace::new();

    let output = run_br(&workspace, ["init"], "init_title_dep");
    assert!(output.status.success(), "init failed");

    // Create a markdown file where issues reference each other by title (bulleted)
    let md_path = workspace.root.join("issues.md");
    let content = r"## Build API Endpoints
### Type
feature
### Dependencies
- Build Database Schema

## Build Database Schema
### Type
task
";
    fs::write(&md_path, content).expect("write md");

    let output = run_br(
        &workspace,
        ["create", "--file", "issues.md", "--json"],
        "create_title_deps_json",
    );
    assert!(
        output.status.success(),
        "create --file --json failed: {}",
        output.stderr
    );

    let payload = extract_json_payload(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&payload).expect("json parse");
    let issues = json.as_array().expect("json array");
    assert_eq!(issues.len(), 2);

    // The first issue (API Endpoints) should depend on the second (Database Schema)
    // This tests forward-reference resolution (dep target defined later in file)
    let db_id = issues[1]["id"].as_str().expect("db issue id");
    let api_deps = issues[0]["dependencies"]
        .as_array()
        .expect("api dependencies array");
    assert_eq!(
        api_deps.len(),
        1,
        "expected 1 dependency, got {}: {api_deps:?}",
        api_deps.len()
    );
    assert_eq!(
        api_deps[0]["depends_on_id"].as_str(),
        Some(db_id),
        "dependency should resolve title 'Build Database Schema' to the generated ID"
    );
}

#[test]
fn test_markdown_import_title_with_colon_dependency_resolution() {
    let workspace = BrWorkspace::new();

    let output = run_br(&workspace, ["init"], "init_colon_title");
    assert!(output.status.success(), "init failed");

    // Titles containing colons must not be misinterpreted as typed deps
    let md_path = workspace.root.join("issues.md");
    let content = r"## Step 1: Setup Database
### Type
task

## Step 2: Build API
### Type
feature
### Dependencies
- Step 1: Setup Database
";
    fs::write(&md_path, content).expect("write md");

    let output = run_br(
        &workspace,
        ["create", "--file", "issues.md", "--json"],
        "create_colon_title_deps_json",
    );
    assert!(
        output.status.success(),
        "create --file --json failed: {}",
        output.stderr
    );

    let payload = extract_json_payload(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&payload).expect("json parse");
    let issues = json.as_array().expect("json array");
    assert_eq!(issues.len(), 2);

    let db_id = issues[0]["id"].as_str().expect("setup issue id");
    let api_deps = issues[1]["dependencies"]
        .as_array()
        .expect("api dependencies array");
    assert_eq!(
        api_deps.len(),
        1,
        "expected 1 dependency (colon in title should not break resolution), got {}: {api_deps:?}",
        api_deps.len()
    );
    assert_eq!(
        api_deps[0]["depends_on_id"].as_str(),
        Some(db_id),
        "dependency should resolve title with colon to the generated ID"
    );
}

#[test]
fn test_markdown_import_ambiguous_duplicate_title_dependency_warns_and_skips() {
    let workspace = BrWorkspace::new();

    let output = run_br(&workspace, ["init"], "init_duplicate_title_dep");
    assert!(output.status.success(), "init failed");

    let md_path = workspace.root.join("issues.md");
    let content = r"## Shared Target
### Type
task

## Dependent
### Type
feature
### Dependencies
- Shared Target

## Shared Target
### Type
bug
";
    fs::write(&md_path, content).expect("write md");

    let output = run_br(
        &workspace,
        ["create", "--file", "issues.md", "--json"],
        "create_duplicate_title_dep_json",
    );
    assert!(
        output.status.success(),
        "create --file --json failed: {}",
        output.stderr
    );
    assert!(
        output
            .stderr
            .contains("ambiguous dependency 'Shared Target'"),
        "expected ambiguous dependency warning, got: {}",
        output.stderr
    );

    let payload = extract_json_payload(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&payload).expect("json parse");
    let issues = json.as_array().expect("json array");
    assert_eq!(issues.len(), 3);

    let dependent = issues
        .iter()
        .find(|issue| issue["title"].as_str() == Some("Dependent"))
        .expect("dependent issue");
    let dep_count = dependent
        .get("dependencies")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    assert_eq!(
        dep_count, 0,
        "ambiguous title dependency should be skipped, got: {dependent:?}"
    );
}

#[test]
fn test_markdown_import_ambiguous_duplicate_standin_dependency_warns_and_skips() {
    let workspace = BrWorkspace::new();

    let output = run_br(&workspace, ["init"], "init_duplicate_standin_dep");
    assert!(output.status.success(), "init failed");

    let md_path = workspace.root.join("issues.md");
    let content = r"## First Target
### ID
target

## Second Target
### ID
target

## Dependent
### Dependencies
- target
";
    fs::write(&md_path, content).expect("write md");

    let output = run_br(
        &workspace,
        ["create", "--file", "issues.md", "--json"],
        "create_duplicate_standin_dep_json",
    );
    assert!(
        output.status.success(),
        "create --file --json failed: {}",
        output.stderr
    );
    assert!(
        output.stderr.contains("ambiguous dependency 'target'"),
        "expected ambiguous dependency warning, got: {}",
        output.stderr
    );

    let payload = extract_json_payload(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&payload).expect("json parse");
    let issues = json.as_array().expect("json array");
    assert_eq!(issues.len(), 3);

    let dependent = issues
        .iter()
        .find(|issue| issue["title"].as_str() == Some("Dependent"))
        .expect("dependent issue");
    let dep_count = dependent
        .get("dependencies")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    assert_eq!(
        dep_count, 0,
        "ambiguous stand-in dependency should be skipped, got: {dependent:?}"
    );
}
