mod common;
use common::cli::{BrWorkspace, run_br, run_br_with_stdin};

#[test]
fn test_comments_add_from_stdin() {
    let _log = common::test_log("test_comments_add_from_stdin");
    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    let create = run_br(&workspace, ["create", "Issue"], "create");
    // Extract ID from "✓ Created bd-1: Issue"
    // Word 0: "✓", Word 1: "Created", Word 2: "bd-1:"
    let id = create
        .stdout
        .split_whitespace()
        .nth(2)
        .unwrap()
        .trim_end_matches(':');

    // Add comment via stdin using '-'
    let add = run_br_with_stdin(
        &workspace,
        ["comments", "add", id, "--file", "-"],
        "This is a comment from stdin",
        "add_stdin",
    );

    if !add.status.success() {
        println!("Add failed: {}", add.stderr);
    }
    assert!(add.status.success());

    // Verify comment
    let list = run_br(&workspace, ["comments", "list", id], "list");
    assert!(list.stdout.contains("This is a comment from stdin"));
}
