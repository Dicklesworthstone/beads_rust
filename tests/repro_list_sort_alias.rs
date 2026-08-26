mod common;

use common::cli::{BrWorkspace, run_br};

#[test]
fn test_list_sort_aliases_are_accepted() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let create = run_br(&workspace, ["create", "sort alias fixture"], "create");
    assert!(
        create.status.success(),
        "fixture create failed: {}",
        create.stderr
    );

    for alias in ["created", "updated"] {
        let list = run_br(
            &workspace,
            ["list", "--sort", alias, "--json"],
            &format!("list_sort_{alias}"),
        );
        assert!(
            list.status.success(),
            "list --sort {alias} failed: stdout={} stderr={}",
            list.stdout,
            list.stderr
        );
    }
}
