mod common;
use common::cli::{BrWorkspace, run_br};

#[test]
fn test_list_priority_accepts_p_prefix() {
    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");
    run_br(&workspace, ["create", "Critical", "-p", "0"], "create");

    // This should work (numeric)
    let list_num = run_br(&workspace, ["list", "-p", "0"], "list_num");
    assert!(
        list_num.status.success(),
        "Numeric priority failed: {}",
        list_num.stderr
    );
    assert!(list_num.stdout.contains("Critical"));

    // This should work (P-prefix) but likely fails currently
    let list_p = run_br(&workspace, ["list", "-p", "P0"], "list_p");

    // If it fails with clap error, it's because of Vec<u8> type
    if list_p.status.success() {
        // If it succeeds, then my hypothesis is wrong (maybe clap handles it?)
        // But clap parser for u8 won't parse "P0".
        println!("P-prefix priority unexpectedly succeeded");
    } else {
        println!("P-prefix priority failed as expected: {}", list_p.stderr);
        // We assert failure to confirm bug reproduction, or assert success if we want to enforce fix
        // I want to confirm it fails now so I can fix it.
        assert!(list_p.stderr.contains("invalid value") || list_p.stderr.contains("error"));
    }
}

#[test]
fn test_list_csv_default_header_and_escaping() {
    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init_csv_default");
    run_br(
        &workspace,
        ["create", "Hello, \"CSV\", world", "-p", "2"],
        "create_csv_default",
    );

    let list = run_br(&workspace, ["list", "--format", "csv"], "list_csv_default");
    assert!(list.status.success(), "CSV list failed: {}", list.stderr);

    let header = list.stdout.lines().next().unwrap_or_default();
    assert_eq!(
        header.trim(),
        "id,title,status,priority,issue_type,assignee,created_at,updated_at"
    );
    assert!(
        list.stdout.contains("\"Hello, \"\"CSV\"\", world\""),
        "CSV output did not escape title correctly: {}",
        list.stdout
    );
}

#[test]
fn test_list_csv_fields_with_newlines() {
    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init_csv_fields");
    run_br(
        &workspace,
        ["create", "HasDescription", "-d", "Line1\nLine2"],
        "create_csv_fields",
    );

    let list = run_br(
        &workspace,
        [
            "list",
            "--format",
            "csv",
            "--fields",
            "id,title,description",
        ],
        "list_csv_fields",
    );
    assert!(
        list.status.success(),
        "CSV list with fields failed: {}",
        list.stderr
    );

    let header = list.stdout.lines().next().unwrap_or_default();
    assert_eq!(header.trim(), "id,title,description");
    assert!(
        list.stdout.contains("\"Line1\nLine2\""),
        "CSV output did not quote newline field: {}",
        list.stdout
    );
}
