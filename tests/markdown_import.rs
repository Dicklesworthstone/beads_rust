mod common;
use common::cli::{BrWorkspace, run_br};
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

    assert!(output.stdout.contains("Created 2 issues:"));
    assert!(output.stdout.contains("bd-"));

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
