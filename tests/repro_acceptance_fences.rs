//! Acceptance edits and prerequisite gates must never act on fenced examples.

mod common;

use common::cli::{BrWorkspace, run_br};
use serde_json::{Value, json};
use std::fs;

const MISLEADING_CLOSE: &str =
    "```markdown\n```not-a-close\n- [x] finished example\n```\n- [ ] unfinished prerequisite\n";
const EXAMPLES_ONLY: &str = "````markdown\n```\n- [x] example only\n```\n````\n";

fn workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(
        &workspace,
        ["init", "--prefix", "fence"],
        "init_fence_workspace",
    );
    assert!(init.status.success(), "{init:?}");
    workspace
}

fn success_json(workspace: &BrWorkspace, args: &[&str], step: &str) -> Value {
    let run = run_br(workspace, args.iter().copied(), step);
    assert!(run.status.success(), "{step}: {run:?}");
    serde_json::from_str(&run.stdout)
        .unwrap_or_else(|error| panic!("whole stdout must be JSON: {error}; {run:?}"))
}

fn show(workspace: &BrWorkspace, id: &str, step: &str) -> Value {
    let value = success_json(workspace, &["show", id, "--json"], step);
    if let Some(rows) = value.as_array() {
        assert_eq!(
            rows.len(),
            1,
            "exactly the requested issue must be returned"
        );
        rows[0].clone()
    } else {
        assert!(value.is_object(), "{value}");
        value
    }
}

fn create(workspace: &BrWorkspace, title: &str, field: &str, body: &str) -> String {
    let row = success_json(
        workspace,
        &["create", title, "--type", "task", field, body, "--json"],
        "create_fence_issue",
    );
    row["id"].as_str().expect("created ID").to_string()
}

fn assert_refused(workspace: &BrWorkspace, args: &[&str], expected: &str, step: &str) {
    let run = run_br(workspace, args.iter().copied(), step);
    assert!(
        !run.status.success(),
        "{step} unexpectedly succeeded: {run:?}"
    );
    let error: Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|error| panic!("whole error stdout must be JSON: {error}; {run:?}"));
    assert!(error["error"].is_object(), "{error}");
    assert!(error.to_string().contains(expected), "{expected}: {error}");
}

fn exported_issue(workspace: &BrWorkspace, id: &str, step: &str) -> Value {
    let flush = run_br(workspace, ["sync", "--flush-only"], step);
    assert!(flush.status.success(), "{flush:?}");
    let jsonl = fs::read_to_string(workspace.root.join(".beads/issues.jsonl")).unwrap();
    jsonl
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("exported JSONL row"))
        .find(|row| row["id"].as_str() == Some(id))
        .expect("requested issue in export")
}

fn configure_prerequisite_policy(workspace: &BrWorkspace) {
    fs::write(
        workspace.root.join(".beads/policy.yaml"),
        "workflow:\n  strict: true\n  statuses: [open, handoff]\n  transitions:\n    initial: [open]\n    open: [handoff]\n  required_fields:\n    handoff: [prerequisites_complete, transition_comment]\n",
    )
    .unwrap();
}

#[test]
fn acceptance_fences_cli_edits_real_items_and_preserves_examples_and_export() {
    let _log = common::test_log(
        "acceptance_fences_cli_edits_real_items_and_preserves_examples_and_export",
    );
    let workspace = workspace();
    let body = "````markdown\r\n```rust\r\n- [ ] example only\r\n```\r\n````\r\n  * [\u{a0}] Réal requirement\r\n  * [X] Existing complete\r\n";
    let id = create(
        &workspace,
        "Real requirements",
        "--acceptance-criteria",
        body,
    );
    let before = show(&workspace, &id, "show_fences_before_edit");
    assert_eq!(before["acceptance_criteria"], body);
    assert_eq!(
        before["acceptance_items"],
        json!([
            {"index": 1, "text": "Réal requirement", "checked": false},
            {"index": 2, "text": "Existing complete", "checked": true}
        ])
    );

    assert_refused(
        &workspace,
        &[
            "update",
            &id,
            "--title",
            "Must not change",
            "--check-acceptance",
            "example only",
            "--json",
        ],
        "no acceptance item matches",
        "refuse_example_selector",
    );
    assert_eq!(show(&workspace, &id, "show_after_bad_selector"), before);

    success_json(
        &workspace,
        &[
            "update",
            &id,
            "--check-acceptance",
            "1",
            "--uncheck-acceptance",
            "2",
            "--add-acceptance",
            "New criterion",
            "--json",
        ],
        "edit_real_requirements",
    );
    let expected = format!(
        "{}* [ ] New criterion\r\n",
        body.replacen("[\u{a0}] Réal", "[x] Réal", 1)
            .replacen("[X] Existing", "[ ] Existing", 1)
    );
    let after = show(&workspace, &id, "show_fences_after_edit");
    assert_eq!(after["acceptance_criteria"], expected);
    assert_eq!(
        after["acceptance_items"],
        json!([
            {"index": 1, "text": "Réal requirement", "checked": true},
            {"index": 2, "text": "Existing complete", "checked": false},
            {"index": 3, "text": "New criterion", "checked": false}
        ])
    );
    assert_eq!(
        exported_issue(&workspace, &id, "export_real_edits")["acceptance_criteria"],
        expected
    );
}

#[test]
fn acceptance_fences_cli_refuses_unclosed_appends_without_partial_mutations() {
    let _log = common::test_log(
        "acceptance_fences_cli_refuses_unclosed_appends_without_partial_mutations",
    );
    let workspace = workspace();
    for (index, ending) in ["```", "````language", "~~~~"].into_iter().enumerate() {
        let body = format!("- [ ] Existing\n````markdown\n{ending}\n- [x] example only\n");
        let id = create(
            &workspace,
            &format!("Unclosed case {index}"),
            "--acceptance-criteria",
            &body,
        );
        let before = show(&workspace, &id, "show_unclosed_before");
        let export_before = exported_issue(&workspace, &id, "export_unclosed_before");
        assert_refused(
            &workspace,
            &[
                "update",
                &id,
                "--title",
                "Must not change",
                "--check-acceptance",
                "1",
                "--add-acceptance",
                "Must not disappear",
                "--json",
            ],
            "unclosed code fence",
            "refuse_unclosed_append",
        );
        assert_eq!(show(&workspace, &id, "show_unclosed_after_refusal"), before);
        assert_eq!(
            exported_issue(&workspace, &id, "export_unclosed_after_refusal"),
            export_before
        );
        // A valid edit before the open fence remains usable, without --force.
        success_json(
            &workspace,
            &["update", &id, "--check-acceptance", "1", "--json"],
            "check_before_open_fence",
        );
        assert_eq!(
            show(&workspace, &id, "show_valid_check")["acceptance_criteria"],
            body.replacen("[ ] Existing", "[x] Existing", 1)
        );
    }
}

#[test]
fn prerequisite_fences_cli_refuses_false_completion_and_commits_real_completion() {
    let _log = common::test_log(
        "prerequisite_fences_cli_refuses_false_completion_and_commits_real_completion",
    );
    let workspace = workspace();
    configure_prerequisite_policy(&workspace);
    let id = create(
        &workspace,
        "Pending preparation",
        "--prerequisites",
        MISLEADING_CLOSE,
    );
    let before = show(&workspace, &id, "show_prerequisites_before");
    let export_before = exported_issue(&workspace, &id, "export_prerequisites_before");
    assert_refused(
        &workspace,
        &[
            "update",
            &id,
            "--status",
            "handoff",
            "--title",
            "Must not change",
            "--transition-comment",
            "Must not persist",
            "--json",
        ],
        "transition_prerequisites_incomplete",
        "refuse_false_prerequisite_completion",
    );
    assert_eq!(
        show(&workspace, &id, "show_prerequisites_after_refusal"),
        before
    );
    assert_eq!(
        exported_issue(&workspace, &id, "export_prerequisites_after_refusal"),
        export_before
    );

    let complete =
        MISLEADING_CLOSE.replace("[ ] unfinished prerequisite", "[x] unfinished prerequisite");
    success_json(
        &workspace,
        &[
            "update",
            &id,
            "--prerequisites",
            &complete,
            "--status",
            "handoff",
            "--transition-comment",
            "Real preparation verified",
            "--json",
        ],
        "complete_and_handoff_atomically",
    );
    let after = show(&workspace, &id, "show_real_completion");
    assert_eq!(after["status"], "handoff");
    assert_eq!(after["title"], before["title"]);
    assert_eq!(after["prerequisites"], complete);
    let exported = exported_issue(&workspace, &id, "export_real_completion");
    assert_eq!(exported["status"], "handoff");
    assert_eq!(exported["prerequisites"], complete);
    assert!(!exported.to_string().contains("Must not persist"));
    let comments = exported["comments"]
        .as_array()
        .expect("committed transition comment");
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["text"], "Real preparation verified");
}

#[test]
fn prerequisite_fences_cli_rejects_example_only_evidence_but_not_unchecked_examples() {
    let _log = common::test_log(
        "prerequisite_fences_cli_rejects_example_only_evidence_but_not_unchecked_examples",
    );
    let workspace = workspace();
    configure_prerequisite_policy(&workspace);
    let only = create(
        &workspace,
        "Examples are not evidence",
        "--prerequisites",
        EXAMPLES_ONLY,
    );
    let before = exported_issue(&workspace, &only, "export_examples_only_before");
    assert_refused(
        &workspace,
        &[
            "update",
            &only,
            "--status",
            "handoff",
            "--transition-comment",
            "Must not persist",
            "--json",
        ],
        "transition_prerequisites_incomplete",
        "refuse_examples_only",
    );
    assert_eq!(
        exported_issue(&workspace, &only, "export_examples_only_after"),
        before
    );

    let complete = format!(
        "{}- [x] Real preparation\n",
        EXAMPLES_ONLY.replace("[x] example", "[ ] example")
    );
    let valid = create(
        &workspace,
        "Genuine completion",
        "--prerequisites",
        &complete,
    );
    success_json(
        &workspace,
        &[
            "update",
            &valid,
            "--status",
            "handoff",
            "--transition-comment",
            "Real preparation verified",
            "--json",
        ],
        "allow_completed_prerequisite_with_unchecked_example",
    );
    let after = show(&workspace, &valid, "show_valid_handoff");
    assert_eq!(after["status"], "handoff");
    assert_eq!(after["prerequisites"], complete);
}
