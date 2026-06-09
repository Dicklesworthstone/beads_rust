mod common;

use common::cli::{BrWorkspace, parse_created_id, parse_json_value, run_br};

fn create_issue(workspace: &BrWorkspace, title: &str, label: &str) -> String {
    let run = run_br(workspace, ["create", title], label);
    assert!(
        run.status.success(),
        "create failed: stdout='{}' stderr='{}'",
        run.stdout,
        run.stderr
    );
    parse_created_id(&run.stdout)
}

fn check<'a>(json: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    json["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|check| check["name"] == name))
        .unwrap_or_else(|| panic!("missing doctor check {name}: {json}"))
}

#[test]
fn doctor_reports_dead_closed_blockers_from_raw_jsonl_and_quick_skips() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let closed = create_issue(&workspace, "Closed blocker", "create_closed");
    let open = create_issue(&workspace, "Open blocker", "create_open");
    let fully = create_issue(&workspace, "Fully unblocked dependent", "create_fully");
    let partial = create_issue(&workspace, "Partially blocked dependent", "create_partial");
    let related = create_issue(&workspace, "Related dependent", "create_related");

    for (label, issue, target, dep_type) in [
        ("dep_fully_closed", &fully, &closed, "blocks"),
        ("dep_partial_closed", &partial, &closed, "blocks"),
        ("dep_partial_open", &partial, &open, "blocks"),
        ("dep_related_closed", &related, &closed, "related"),
    ] {
        let run = run_br(
            &workspace,
            ["dep", "add", issue, target, "--type", dep_type],
            label,
        );
        assert!(
            run.status.success(),
            "{label} failed: stdout='{}' stderr='{}'",
            run.stdout,
            run.stderr
        );
    }

    let close = run_br(
        &workspace,
        ["close", &closed, "--reason", "synthetic blocker closed"],
        "close_blocker",
    );
    assert!(
        close.status.success(),
        "close failed: stdout='{}' stderr='{}'",
        close.stdout,
        close.stderr
    );

    let doctor = run_br(&workspace, ["doctor", "--json"], "doctor_json");
    assert!(
        !doctor.status.success(),
        "doctor should exit with findings for dead closed blockers"
    );
    let json = parse_json_value(&doctor.stdout);

    let dead = check(&json, "dep.dead_closed_blocking_edges");
    assert_eq!(dead["status"], "warn", "{dead}");
    assert_eq!(dead["details"]["dead_closed_edge_count"], 2);

    let fully_check = check(&json, "dep.fully_unblocked_open");
    assert_eq!(fully_check["status"], "warn", "{fully_check}");
    assert_eq!(fully_check["details"]["fully_unblocked_open_count"], 1);
    assert!(
        fully_check["details"]["sample_issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["issue_id"] == fully),
        "fully-unblocked sample should name the dependent: {fully_check}"
    );

    let quick = run_br(
        &workspace,
        ["doctor", "--quick", "--json"],
        "doctor_quick_json",
    );
    let quick_json = parse_json_value(&quick.stdout);
    let check_names: Vec<_> = quick_json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|check| check["name"].as_str())
        .collect();
    assert!(!check_names.contains(&"dep.dead_closed_blocking_edges"));
    assert!(!check_names.contains(&"dep.fully_unblocked_open"));
}
