mod common;
use common::cli::{BrWorkspace, extract_json_payload, parse_created_id, parse_list_issues, run_br};
use std::collections::HashSet;

#[test]
fn test_ready_limit_with_external_blockers() {
    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    // Create 10 issues
    for i in 1..=10 {
        run_br(
            &workspace,
            ["create", &format!("Issue {i}")],
            &format!("create_{i}"),
        );
    }

    // Block the first 5 with external dependencies (that won't resolve)
    // IDs are likely bd-1 to bd-10 (base36).
    // bd-1, bd-2, bd-3, bd-4, bd-5.
    // bd-6..10 are free.

    // We need actual IDs.
    // Assuming deterministic IDs or extracting them.
    // For simplicity, let's just grep list output or use `br list --json` to get IDs.
    let list = run_br(&workspace, ["list", "--json"], "list");
    let issues = parse_list_issues(&list.stdout);

    let mut ids: Vec<String> = issues
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_string())
        .collect();
    // Sort IDs to ensure we block the first created ones (which ready returns first)
    ids.sort();

    assert_eq!(ids.len(), 10);

    for (i, id) in ids.iter().take(5).enumerate() {
        run_br(
            &workspace,
            ["dep", "add", id.as_str(), "external:missing:dep"],
            &format!("block_{i}"),
        );
    }

    // Run ready with limit 5
    // We expect it to skip the 5 blocked ones and return the next 5.
    let ready = run_br(&workspace, ["ready", "--limit", "5", "--json"], "ready");
    let ready_issues: Vec<serde_json::Value> = serde_json::from_str(&ready.stdout).unwrap();

    // If bug exists, this will likely be 0 (or < 5).
    // If fixed, should be 5.
    assert_eq!(
        ready_issues.len(),
        5,
        "Expected 5 ready issues, got {}",
        ready_issues.len()
    );

    // Verify we got the unblocked ones
    for issue in ready_issues {
        let id = issue["id"].as_str().unwrap();
        assert!(
            !ids[0..5].contains(&id.to_string()),
            "Blocked issue {id} returned in ready list"
        );
    }
}

#[test]
fn ready_default_is_unlimited_and_does_not_hide_ready_p0s() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let mut created_ids = HashSet::new();
    for index in 0..25 {
        let title = format!("Ready P0 measurement issue {index:02}");
        let create = run_br(
            &workspace,
            ["create", title.as_str(), "-p", "0"],
            &format!("create_ready_p0_{index:02}"),
        );
        assert!(
            create.status.success(),
            "create ready issue {index} failed: {}",
            create.stderr
        );
        assert!(created_ids.insert(parse_created_id(&create.stdout)));
    }

    let ready = run_br(&workspace, ["ready", "--json"], "ready_default_unlimited");
    assert!(ready.status.success(), "ready failed: {}", ready.stderr);
    let payload = extract_json_payload(&ready.stdout);
    let ready_issues: Vec<serde_json::Value> =
        serde_json::from_str(&payload).expect("ready JSON array");

    assert_eq!(
        ready_issues.len(),
        created_ids.len(),
        "default ready must not silently truncate ready P0s"
    );

    let returned_ids: HashSet<String> = ready_issues
        .iter()
        .map(|issue| issue["id"].as_str().expect("ready issue id").to_string())
        .collect();
    assert_eq!(returned_ids, created_ids);
}
