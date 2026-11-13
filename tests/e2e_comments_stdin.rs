mod common;
use common::cli::{BrWorkspace, extract_json_payload, run_br, run_br_with_stdin};
use serde_json::Value;

fn created_id(stdout: &str) -> &str {
    stdout
        .lines()
        .find_map(|line| line.split_once("Created "))
        .and_then(|(_, rest)| rest.split_once(':'))
        .map(|(id, _)| id.trim())
        .expect("created issue id")
}

#[test]
fn test_comments_add_from_stdin() {
    let _log = common::test_log("test_comments_add_from_stdin");
    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    let create = run_br(&workspace, ["create", "Issue"], "create");
    let id = created_id(&create.stdout);

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

#[test]
#[allow(clippy::too_many_lines)]
fn test_comments_edit_from_stdin_and_file_preserves_exact_text() {
    let _log = common::test_log("test_comments_edit_from_stdin_and_file_preserves_exact_text");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "{}", init.stderr);
    let create = run_br(&workspace, ["create", "Input formats"], "create");
    assert!(create.status.success(), "{}", create.stderr);
    let issue_id = created_id(&create.stdout);
    let add = run_br(
        &workspace,
        [
            "comments",
            "add",
            issue_id,
            "original text",
            "--actor",
            "alice",
            "--json",
        ],
        "add",
    );
    assert!(add.status.success(), "{}", add.stderr);
    let original: Value = serde_json::from_str(&extract_json_payload(&add.stdout)).unwrap();
    let comment_id = original["id"].as_i64().unwrap().to_string();
    let replacement = "  Preserve indentation\nUnicode: café ✓\n";
    let edit = run_br_with_stdin(
        &workspace,
        [
            "comments",
            "edit",
            issue_id,
            &comment_id,
            "--file",
            "-",
            "--actor",
            "alice",
            "--json",
        ],
        replacement,
        "edit_stdin",
    );
    assert!(edit.status.success(), "{}", edit.stderr);
    let from_stdin: Value = serde_json::from_str(&extract_json_payload(&edit.stdout)).unwrap();
    assert_eq!(from_stdin["text"], replacement);
    assert_eq!(
        from_stdin["revision"]["identity"],
        original["revision"]["identity"]
    );

    let file = workspace.root.join("replacement-comment.txt");
    let file_text = "A second line source\n\tTabs and trailing newline\n";
    std::fs::write(&file, file_text).unwrap();
    let edit = run_br(
        &workspace,
        [
            "comments",
            "edit",
            issue_id,
            &comment_id,
            "--file",
            file.to_str().unwrap(),
            "--actor",
            "alice",
            "--json",
        ],
        "edit_file",
    );
    assert!(edit.status.success(), "{}", edit.stderr);
    let from_file: Value = serde_json::from_str(&extract_json_payload(&edit.stdout)).unwrap();
    assert_eq!(from_file["text"], file_text);
    assert_eq!(from_file["author"], "alice");
    assert_eq!(from_file["created_at"], original["created_at"]);
    assert_eq!(from_file["id"], original["id"]);
    assert_eq!(
        from_file["revision"]["identity"],
        original["revision"]["identity"]
    );

    let empty = run_br_with_stdin(
        &workspace,
        [
            "comments",
            "edit",
            issue_id,
            &comment_id,
            "--file",
            "-",
            "--actor",
            "alice",
        ],
        " \n\t",
        "reject_empty_stdin",
    );
    assert!(!empty.status.success());
    let conflict = run_br(
        &workspace,
        [
            "comments",
            "edit",
            issue_id,
            &comment_id,
            "positional text",
            "--file",
            file.to_str().unwrap(),
            "--actor",
            "alice",
        ],
        "reject_conflicting_inputs",
    );
    assert!(!conflict.status.success());
    let list = run_br(
        &workspace,
        ["comments", "list", issue_id, "--json"],
        "unchanged_after_invalid_input",
    );
    assert!(list.status.success(), "{}", list.stderr);
    let comments: Value = serde_json::from_str(&extract_json_payload(&list.stdout)).unwrap();
    assert_eq!(comments, serde_json::json!([from_file]));
}
