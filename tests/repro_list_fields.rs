//! End-to-end coverage for explicit JSON/TOON list field selection.

mod common;

use common::cli::{BrWorkspace, run_br};
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init_list_fields");
    assert!(init.status.success(), "{init:?}");
    workspace
}

fn create(workspace: &BrWorkspace, title: &str, description: &str) -> String {
    let result = run_br(
        workspace,
        [
            "create",
            title,
            "--type",
            "task",
            "--description",
            description,
        ],
        "create_list_fields_issue",
    );
    assert!(result.status.success(), "{result:?}");
    let line = result.stdout.lines().next().expect("creation output");
    let normalized = line.strip_prefix("✓ ").unwrap_or(line);
    normalized
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .expect("created issue ID")
        .trim()
        .to_string()
}

fn page(workspace: &BrWorkspace, extra: &[&str], step: &str) -> Value {
    let mut args = vec!["list", "--json"];
    args.extend_from_slice(extra);
    let result = run_br(workspace, args, step);
    assert!(result.status.success(), "{result:?}");
    // A structured list must be one complete JSON document, not prose with an
    // embedded payload that an extraction helper could silently forgive.
    serde_json::from_str(&result.stdout).expect("whole list stdout is JSON")
}

fn assert_projection(full: &Value, selected: &Value, fields: &[&str]) {
    for key in ["total", "limit", "offset", "has_more"] {
        assert_eq!(selected[key], full[key], "page metadata {key}");
    }
    let full_rows = full["issues"].as_array().expect("full rows");
    let selected_rows = selected["issues"].as_array().expect("selected rows");
    assert_eq!(full_rows.len(), selected_rows.len());
    let expected_keys = fields.iter().copied().collect::<BTreeSet<_>>();
    for (expected, actual) in full_rows.iter().zip(selected_rows) {
        let actual_keys = actual
            .as_object()
            .expect("selected object")
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        assert_eq!(actual_keys, expected_keys);
        for &field in fields {
            let expected_value = expected.get(field).cloned().unwrap_or_else(|| {
                if field == "labels" {
                    json!([])
                } else {
                    Value::Null
                }
            });
            assert_eq!(actual[field], expected_value, "{field}");
        }
    }
}

#[test]
fn list_fields_preserve_rows_types_and_explicit_wide_columns() {
    let _log = common::test_log("list_fields_preserve_rows_types_and_explicit_wide_columns");
    let workspace = workspace();
    let description = "BODY_ONLY_UNSELECTED ".repeat(512);
    let alpha = create(&workspace, "Alpha", &description);
    create(&workspace, "Beta", "Another description");
    create(&workspace, "Gamma", "Last description");
    let update = run_br(
        &workspace,
        [
            "update",
            &alpha,
            "--notes",
            "NOTES_ONLY_UNSELECTED",
            "--acceptance-criteria",
            "- [ ] Exact criterion",
        ],
        "set_list_fields_long_values",
    );
    assert!(update.status.success(), "{update:?}");
    let full = page(&workspace, &["--sort", "title"], "list_fields_full");
    assert_eq!(full["total"], 3);
    assert_eq!(full["limit"], 0);
    for (index, fields) in [
        "id,title,status,priority,issue_type",
        "title, id,title,priority",
        "id,description,notes,acceptance_criteria,assignee,updated_at",
    ]
    .into_iter()
    .enumerate()
    {
        let selected = page(
            &workspace,
            &["--sort", "title", "--fields", fields],
            &format!("list_fields_projection_{index}"),
        );
        let keys = fields.split(',').map(str::trim).collect::<Vec<_>>();
        assert_projection(&full, &selected, &keys);
        if index == 0 {
            let encoded = serde_json::to_string(&selected).unwrap();
            assert!(!encoded.contains("BODY_ONLY_UNSELECTED"));
            assert!(!encoded.contains("NOTES_ONLY_UNSELECTED"));
        }
    }
    // Omitting --fields still returns the original full schema and values.
    assert_eq!(
        full,
        page(&workspace, &["--sort", "title"], "list_fields_full_again")
    );
}

#[test]
fn list_fields_apply_client_filters_before_offset_and_limit() {
    let _log = common::test_log("list_fields_apply_client_filters_before_offset_and_limit");
    let workspace = workspace();
    create(&workspace, "Alpha excluded", "No match");
    create(&workspace, "Beta", "needle");
    let gamma = create(&workspace, "Gamma", "NEEDLE");
    for (index, offset) in ["0", "1", "2", "99"].into_iter().enumerate() {
        let extra = [
            "--desc-contains",
            "needle",
            "--sort",
            "title",
            "--limit",
            "1",
            "--offset",
            offset,
        ];
        let full = page(
            &workspace,
            &extra,
            &format!("list_fields_filter_full_{index}"),
        );
        let mut projected = extra.to_vec();
        projected.extend(["--fields", "id,title"]);
        let selected = page(
            &workspace,
            &projected,
            &format!("list_fields_filter_{index}"),
        );
        assert_projection(&full, &selected, &["id", "title"]);
        assert_eq!(selected["total"], 2);
        assert_eq!(selected["has_more"], offset == "0");
        if offset == "1" {
            assert_eq!(selected["issues"][0]["id"], gamma);
        }
    }
    let full = page(
        &workspace,
        &["--desc-contains", "needle", "--sort", "title", "--reverse"],
        "list_fields_reverse_full",
    );
    let selected = page(
        &workspace,
        &[
            "--desc-contains",
            "needle",
            "--sort",
            "title",
            "--reverse",
            "--fields",
            "id",
        ],
        "list_fields_reverse",
    );
    assert_projection(&full, &selected, &["id"]);
    assert_eq!(selected["issues"][0]["id"], gamma);
}

#[test]
#[allow(clippy::too_many_lines)]
fn list_fields_retain_relations_and_support_toon_without_changing_text_or_csv() {
    let _log = common::test_log("list_fields_retain_relations_and_support_toon");
    let workspace = workspace();
    let first = create(&workspace, "Alpha", "UNSELECTED_LONG_BODY");
    let second = create(&workspace, "Beta", "UNSELECTED_LONG_BODY");
    let label = run_br(
        &workspace,
        ["label", "add", &first, "backend"],
        "list_fields_label",
    );
    assert!(label.status.success(), "{label:?}");
    let dep = run_br(
        &workspace,
        ["dep", "add", &first, &second],
        "list_fields_dependency",
    );
    assert!(dep.status.success(), "{dep:?}");
    let full = page(
        &workspace,
        &["--sort", "title"],
        "list_fields_relations_full",
    );
    for (index, fields) in ["id,labels", "id,dependency_count,dependent_count"]
        .into_iter()
        .enumerate()
    {
        let selected = page(
            &workspace,
            &["--sort", "title", "--fields", fields],
            &format!("list_fields_relations_{index}"),
        );
        assert_projection(&full, &selected, &fields.split(',').collect::<Vec<_>>());
    }
    assert_eq!(full["issues"][0]["labels"], json!(["backend"]));
    assert_eq!(full["issues"][0]["dependency_count"], 1);
    assert_eq!(full["issues"][1]["dependent_count"], 1);
    let toon = run_br(
        &workspace,
        [
            "list", "--format", "toon", "--fields", "id,title", "--sort", "title",
        ],
        "list_fields_toon",
    );
    assert!(toon.status.success(), "{toon:?}");
    assert!(toon.stdout.contains(&first), "{toon:?}");
    assert!(toon.stdout.contains(&second), "{toon:?}");
    assert!(toon.stdout.contains("total: 2"), "{toon:?}");
    assert!(!toon.stdout.contains("UNSELECTED_LONG_BODY"), "{toon:?}");
    assert!(!toon.stdout.contains("description"), "{toon:?}");
    let plain = run_br(&workspace, ["list", "--no-color"], "list_fields_plain");
    let ignored = run_br(
        &workspace,
        ["list", "--no-color", "--fields", "id"],
        "list_fields_plain_ignored",
    );
    assert!(plain.status.success() && ignored.status.success());
    assert_eq!(plain.stdout, ignored.stdout);
    let csv = run_br(
        &workspace,
        [
            "list", "--format", "csv", "--fields", "id,title", "--sort", "title",
        ],
        "list_fields_csv",
    );
    assert!(csv.status.success(), "{csv:?}");
    assert!(csv.stdout.contains(&first) && csv.stdout.contains(&second));
    assert!(!csv.stdout.contains("UNSELECTED_LONG_BODY"));
    // `--json` outranks `--quiet`: the documented mode-detection order checks
    // `--json`/`--robot` before `--quiet` (AGENTS.md "Mode Detection", README
    // "Output Modes"), and shipped br 0.6.0 behaves that way — `br list --json
    // --quiet` prints the page while `br list --quiet` prints nothing.
    //
    // So this combination cannot print nothing, and asserting that it does was
    // unsatisfiable rather than merely wrong: `--fields` is only applied in
    // Json/Toon mode (src/cli/commands/list.rs:88), so the very flag that makes
    // `--fields` meaningful is the one that suppresses quiet. Pin the real
    // contract instead — the page is still emitted, and still projected.
    let quiet = run_br(
        &workspace,
        ["list", "--json", "--fields", "id", "--quiet"],
        "list_fields_quiet",
    );
    assert!(quiet.status.success(), "{quiet:?}");
    assert!(
        quiet.stdout.contains(&first) && quiet.stdout.contains(&second),
        "--json outranks --quiet, so the selected page must still be printed: {quiet:?}"
    );
    assert!(
        !quiet.stdout.contains("UNSELECTED_LONG_BODY"),
        "--fields must still project when --quiet is also passed: {quiet:?}"
    );
}

#[test]
fn list_fields_reject_invalid_selectors_even_with_an_empty_result() {
    let _log = common::test_log("list_fields_reject_invalid_selectors_even_with_an_empty_result");
    let workspace = workspace();
    for (index, fields) in ["id,typo", "id,,title", "id,"].into_iter().enumerate() {
        let result = run_br(
            &workspace,
            ["list", "--json", "--fields", fields],
            &format!("list_fields_invalid_{index}"),
        );
        assert_eq!(result.status.code(), Some(4), "{result:?}");
        assert!(result.stdout.contains("fields") || result.stderr.contains("fields"));
    }
    let empty = page(&workspace, &["--fields", "id,title"], "list_fields_empty");
    assert_eq!(empty["issues"], json!([]));
    assert_eq!(empty["total"], 0);
    assert_eq!(empty["limit"], 0);
    assert_eq!(empty["has_more"], false);
}

#[test]
fn list_fields_do_not_hide_work_at_the_large_page_threshold() {
    let _log = common::test_log("list_fields_do_not_hide_work_at_the_large_page_threshold");
    let workspace = workspace();
    for index in 0..101 {
        create(
            &workspace,
            &format!("Item {index:03}"),
            "Long body not needed for selection",
        );
    }
    let all = page(&workspace, &["--fields", "id,title"], "list_fields_all_101");
    assert_eq!(all["issues"].as_array().unwrap().len(), 101);
    assert_eq!(all["total"], 101);
    assert_eq!(all["limit"], 0);
    assert_eq!(all["has_more"], false);
    for (index, extra) in [
        vec!["--limit", "96"],
        vec!["--limit", "2", "--offset", "98"],
        vec!["--limit", "0", "--offset", "99"],
    ]
    .into_iter()
    .enumerate()
    {
        let full = page(
            &workspace,
            &extra,
            &format!("list_fields_large_full_{index}"),
        );
        let mut projected = extra;
        projected.extend(["--fields", "id,title"]);
        let selected = page(
            &workspace,
            &projected,
            &format!("list_fields_large_{index}"),
        );
        assert_projection(&full, &selected, &["id", "title"]);
        assert_eq!(selected["total"], 101);
    }
}
