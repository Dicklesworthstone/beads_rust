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
    let content = r###"## First Issue
### Priority
1
### Labels
bug, frontend

## Second Issue
Implicit description here.

### Type
feature
### Dependencies
blocks:First Issue
"###;
    // Note: Dependencies refer to IDs, but "First Issue" isn't an ID.
    // However, if we use dry-run first, we don't know IDs.
    // But wait, my implementation parses "blocks:First Issue". "First Issue" is NOT a valid ID format (bd-...).
    // So `add_dependency` will likely fail validation if it checks ID format?
    // Or if `blocks:First Issue` is treated as external?
    // `sqlite.rs` doesn't validate ID format for deps strictly, but `detect_collision` does?
    // Wait, `add_dependency` calls `would_create_cycle`.
    // It doesn't strictly validate target ID format if not enforcing.
    // BUT `parse_id` might be called somewhere.
    // Let's stick to valid IDs if possible, but we don't know them yet.
    // The markdown import feature usually implies creating new issues.
    // Linking them requires knowing IDs or using a "placeholder" mechanism which isn't implemented here.
    // So let's remove the dependency for now to ensure basic creation works.

    let content_safe = r###"## First Issue
### Priority
1
### Labels
bug, frontend

## Second Issue
Implicit description here.

### Type
feature
"###;

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
    // "feature" type usually shows badge or text?

    // Verify labels on First Issue
    // We need to know ID or grep.
    // List output usually doesn't show labels in short mode?
    // Use --long or show.

    // Get ID of First Issue
    // list output: "1. [P1] bd-xxx First Issue"
    let id_line = output
        .stdout
        .lines()
        .find(|l| l.contains("First Issue"))
        .expect("found issue");
    let parts: Vec<&str> = id_line.split_whitespace().collect();
    // parts[0] = index, parts[1] = priority, parts[2] = ID
    // depending on formatting.
    // "[P1]" might be part 1.
    // Let's just `show` all IDs from list.

    // Actually, `br list --json` is easier.
    let output = run_br(&workspace, ["list", "--json"], "list_json");
    assert!(output.status.success());

    // We can just text search JSON.
    assert!(output.stdout.contains(r#""title": "First Issue"#));
    assert!(output.stdout.contains(r#""labels": ["#));
    assert!(output.stdout.contains(r#""bug"#));
    assert!(output.stdout.contains(r#""frontend"#));
}
