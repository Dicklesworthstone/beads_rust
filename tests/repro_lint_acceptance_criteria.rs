//! Regression coverage for #509: lint must read acceptance_criteria and must
//! not mistake a prose mention or a Markdown example for legacy criteria.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::{Value, json};

fn initialized_workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "{init:?}");
    workspace
}

fn create_issue(
    workspace: &BrWorkspace,
    title: &str,
    issue_type: &str,
    description: Option<&str>,
    criteria: Option<&str>,
) -> String {
    let mut args = vec!["create", title, "--type", issue_type];
    if let Some(description) = description {
        args.extend(["--description", description]);
    }
    if let Some(criteria) = criteria {
        args.extend(["--acceptance-criteria", criteria]);
    }
    let created = run_br(workspace, args, &format!("create_{issue_type}"));
    assert!(created.status.success(), "{created:?}");
    let line = created.stdout.lines().next().expect("creation output");
    let normalized = line.strip_prefix("✓ ").unwrap_or(line);
    let id = normalized
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .expect("created issue ID")
        .trim();
    assert!(!id.is_empty(), "{created:?}");
    id.to_string()
}

fn lint_json(workspace: &BrWorkspace, ids: &[&str], step: &str) -> Value {
    let mut args = vec!["lint"];
    args.extend_from_slice(ids);
    args.push("--json");
    let lint = run_br(workspace, args, step);
    // Structured output preserves the existing exit-zero contract, even when
    // warnings are present. Consumers inspect total/results instead.
    assert!(lint.status.success(), "{lint:?}");
    serde_json::from_str(&extract_json_payload(&lint.stdout)).expect("lint JSON")
}

fn assert_missing_acceptance(output: &Value, id: &str) {
    assert_eq!(output["total"], 1, "{output}");
    assert_eq!(output["issues"], 1, "{output}");
    let results = output["results"].as_array().expect("results array");
    assert_eq!(results.len(), 1, "{output}");
    assert_eq!(results[0]["id"], id);
    assert_eq!(results[0]["warnings"], 1);
    assert_eq!(results[0]["missing"], json!(["## Acceptance Criteria"]));
    assert_eq!(
        results[0]["suggestions"][0]["section"],
        "## Acceptance Criteria"
    );
}

#[test]
fn lint_accepts_documented_fields_for_tasks_features_and_bugs() {
    let _log = common::test_log("lint_accepts_documented_fields_for_tasks_features_and_bugs");
    let workspace = initialized_workspace();
    for (issue_type, description) in [
        ("task", None),
        ("feature", None),
        ("bug", Some("## Steps to Reproduce\n1. Trigger the bug")),
    ] {
        let id = create_issue(
            &workspace,
            &format!("Field-only criteria for {issue_type}"),
            issue_type,
            description,
            Some("- [ ] the thing works"),
        );
        let specific = run_br(&workspace, ["lint", &id, "--no-color"], "lint_field_id");
        assert!(specific.status.success(), "{specific:?}");
        assert_eq!(lint_json(&workspace, &[&id], "lint_field_id_json")["total"], 0);
    }

    let all = run_br(&workspace, ["lint", "--no-color"], "lint_fields_all");
    assert!(all.status.success(), "{all:?}");
    assert!(all.stdout.contains("No template warnings found"), "{all:?}");
    let quiet = run_br(&workspace, ["lint", "--quiet"], "lint_fields_quiet");
    assert!(quiet.status.success(), "{quiet:?}");
    assert!(quiet.stdout.is_empty(), "{quiet:?}");
    let output = lint_json(&workspace, &[], "lint_fields_json");
    assert_eq!(output["total"], 0);
    assert_eq!(output["issues"], 0);
    assert_eq!(output["results"], json!([]));
}

#[test]
fn lint_reports_prose_control_not_the_issue_with_field_criteria() {
    let _log = common::test_log("lint_reports_prose_control_not_the_issue_with_field_criteria");
    let workspace = initialized_workspace();
    let valid = create_issue(
        &workspace,
        "Criteria in the field",
        "task",
        None,
        Some("- [ ] the thing works"),
    );
    let invalid = create_issue(
        &workspace,
        "No criteria at all",
        "task",
        Some("We will add acceptance criteria later."),
        None,
    );

    let output = lint_json(&workspace, &[], "lint_both_directions");
    assert_missing_acceptance(&output, &invalid);
    let specific = lint_json(&workspace, &[&valid, &invalid], "lint_both_ids");
    assert_eq!(specific, output);

    let text = run_br(&workspace, ["lint", "--no-color"], "lint_prose_text");
    assert_eq!(text.status.code(), Some(1), "{text:?}");
    assert!(text.stdout.contains(&invalid), "{text:?}");
    assert!(!text.stdout.contains(&valid), "{text:?}");
    let quiet = run_br(&workspace, ["lint", "--quiet"], "lint_prose_quiet");
    assert_eq!(quiet.status.code(), Some(1), "{quiet:?}");
    assert!(quiet.stdout.is_empty(), "{quiet:?}");
    let specific_text = run_br(
        &workspace,
        ["lint", &invalid, "--no-color"],
        "lint_prose_id_text",
    );
    assert_eq!(specific_text.status.code(), Some(1), "{specific_text:?}");
}

#[test]
fn lint_bug_control_requires_the_field_and_accepts_update_criteria() {
    let _log = common::test_log("lint_bug_control_requires_the_field_and_accepts_update_criteria");
    let workspace = initialized_workspace();
    let description = "Control issue intentionally created without acceptance criteria.\n\n\
                       ## Steps to Reproduce\n1. Confirm the field is absent.";
    let id = create_issue(
        &workspace,
        "Bug control without criteria",
        "bug",
        Some(description),
        None,
    );
    assert_missing_acceptance(&lint_json(&workspace, &[], "lint_bug_control"), &id);
    assert_missing_acceptance(&lint_json(&workspace, &[&id], "lint_bug_control_id"), &id);

    let criteria = "- [ ] Missing criteria are reported\n- [ ] Stored criteria are accepted";
    let updated = run_br(
        &workspace,
        ["update", &id, "--acceptance-criteria", criteria],
        "update_criteria",
    );
    assert!(updated.status.success(), "{updated:?}");
    let shown = run_br(&workspace, ["show", &id, "--json"], "show_criteria");
    assert!(shown.status.success(), "{shown:?}");
    let issue: Value =
        serde_json::from_str(&extract_json_payload(&shown.stdout)).expect("show JSON");
    assert_eq!(issue[0]["acceptance_criteria"], criteria);
    assert_eq!(issue[0]["description"], description);
    assert_eq!(lint_json(&workspace, &[], "lint_after_update")["total"], 0);
    assert_eq!(lint_json(&workspace, &[&id], "lint_id_after_update")["total"], 0);
    let text = run_br(&workspace, ["lint", "--no-color"], "lint_update_text");
    assert!(text.status.success(), "{text:?}");
}

#[test]
fn lint_legacy_fallback_requires_a_real_heading_and_body() {
    let _log = common::test_log("lint_legacy_fallback_requires_a_real_heading_and_body");
    let workspace = initialized_workspace();
    let legacy = create_issue(
        &workspace,
        "Legacy criteria",
        "task",
        Some("# acceptance criteria:\r\n- [ ] The thing works"),
        None,
    );
    assert_eq!(lint_json(&workspace, &[&legacy], "lint_legacy")["total"], 0);

    let mut invalid_ids = Vec::new();
    for (title, description) in [
        ("Fenced example", "```markdown\n## Acceptance Criteria\n- Example\n```"),
        ("Empty section", "## Acceptance Criteria\n\n## Notes\nUnrelated content"),
        ("Heading lookalike", "## Acceptance Criteria backlog\n- Not a criteria section"),
    ] {
        let id = create_issue(&workspace, title, "task", Some(description), None);
        assert_missing_acceptance(&lint_json(&workspace, &[&id], "lint_invalid_legacy"), &id);
        invalid_ids.push(id);
    }

    let output = lint_json(&workspace, &[], "lint_legacy_workspace");
    assert_eq!(output["total"], 3);
    assert_eq!(output["issues"], 3);
    let results = output["results"].as_array().expect("results array");
    assert_eq!(results.len(), 3);
    for id in invalid_ids {
        let result = results.iter().find(|result| result["id"] == id).unwrap();
        assert_eq!(result["missing"], json!(["## Acceptance Criteria"]));
        assert_eq!(result["warnings"], 1);
    }
    assert!(results.iter().all(|result| result["id"] != legacy));
}
