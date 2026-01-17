mod common;
use common::cli::{BrWorkspace, extract_json_payload, run_br};
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
    assert!(output.stdout.contains("✓ bd-"));

    // Verify list
    let output = run_br(&workspace, ["list"], "list");
    assert!(output.status.success());
    assert!(output.stdout.contains("First Issue"));
    assert!(output.stdout.contains("Second Issue"));
    assert!(output.stdout.contains("[P1]")); // Priority 1

    // Verify labels on First Issue using JSON output
    let output = run_br(&workspace, ["list", "--json"], "list_json");
    assert!(output.status.success());

    assert!(output.stdout.contains(r#""title": "First Issue"#));
    assert!(output.stdout.contains(r#""labels": ["#));
    assert!(output.stdout.contains(r#""bug"#));
    assert!(output.stdout.contains(r#""frontend"#));
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

#[test]
fn test_markdown_import_rejects_dry_run() {
    let workspace = BrWorkspace::new();

    let output = run_br(&workspace, ["init"], "init_dry_run");
    assert!(output.status.success(), "init failed");

    let md_path = workspace.root.join("issues.md");
    let content = r"## DryRun Issue
### Type
task
";
    fs::write(&md_path, content).expect("write md");

    let output = run_br(
        &workspace,
        ["create", "--file", "issues.md", "--dry-run"],
        "create_dry_run",
    );
    assert!(!output.status.success(), "dry-run should fail with --file");
    assert!(
        output
            .stderr
            .contains("--dry-run is not supported with --file")
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
            .contains("warning: skipping invalid dependency type"),
        "expected warning for invalid dependency type"
    );
}
