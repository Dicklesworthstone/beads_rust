//! End-to-end field selection through the actual search command.

mod common;

use common::cli::{BrWorkspace, run_br};
use serde_json::{Value, json};

fn workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let result = run_br(&workspace, ["init"], "init_search_fields");
    assert!(result.status.success(), "{result:?}");
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
        "create_search_fields_issue",
    );
    assert!(result.status.success(), "{result:?}");
    let line = result.stdout.lines().next().expect("creation output");
    line.strip_prefix("✓ ")
        .unwrap_or(line)
        .strip_prefix("Created ")
        .and_then(|text| text.split(':').next())
        .expect("created issue ID")
        .trim()
        .to_string()
}

fn page(workspace: &BrWorkspace, query: &str, extra: &[&str], step: &str) -> Value {
    let mut args = vec!["search", query, "--json"];
    args.extend_from_slice(extra);
    let result = run_br(workspace, args, step);
    assert!(result.status.success(), "{result:?}");
    serde_json::from_str(&result.stdout).expect("the entire stdout must be JSON")
}

fn assert_projection(full: &Value, selected: &Value, fields: &str) {
    let mut expected = full.clone();
    expected["issues"] = Value::Array(
        full["issues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                let mut object = serde_json::Map::new();
                for field in fields.split(',').map(str::trim) {
                    object.insert(
                        field.to_string(),
                        row.get(field).cloned().unwrap_or_else(|| {
                            if field == "labels" {
                                json!([])
                            } else {
                                Value::Null
                            }
                        }),
                    );
                }
                Value::Object(object)
            })
            .collect(),
    );
    assert_eq!(selected, &expected);
    assert!(selected.get("total").is_none());
}

#[test]
fn search_fields_cli_keeps_matches_relations_and_explicit_long_columns() {
    let _log =
        common::test_log("search_fields_cli_keeps_matches_relations_and_explicit_long_columns");
    let workspace = workspace();
    let first = create(
        &workspace,
        "Alpha",
        &format!("needle {}", "BODY_ONLY_MARKER ".repeat(256)),
    );
    let second = create(&workspace, "Beta", "Only comment history matches");
    let comment = run_br(
        &workspace,
        [
            "comments",
            "add",
            &second,
            "--message",
            "NEEDLE durable handoff",
        ],
        "search_fields_old_comment",
    );
    assert!(comment.status.success(), "{comment:?}");
    let recent = run_br(
        &workspace,
        [
            "comments",
            "add",
            &second,
            "--message",
            "Unrelated newer handoff",
        ],
        "search_fields_recent_comment",
    );
    assert!(recent.status.success(), "{recent:?}");
    let label = run_br(
        &workspace,
        ["label", "add", &first, "backend"],
        "search_fields_label",
    );
    assert!(label.status.success(), "{label:?}");
    let dep = run_br(
        &workspace,
        ["dep", "add", &first, &second],
        "search_fields_dep",
    );
    assert!(dep.status.success(), "{dep:?}");
    let full = page(
        &workspace,
        "needle",
        &["--sort", "title"],
        "search_fields_full",
    );
    assert_eq!(full["issues"].as_array().unwrap().len(), 2);
    assert_eq!(full["issues"][0]["dependency_count"], 1);
    assert_eq!(full["issues"][1]["dependent_count"], 1);
    for (index, fields) in [
        "id,title,status,priority,issue_type",
        "id,title,labels,dependency_count,dependent_count",
        " title,id,title ",
        "id,description,notes,assignee,updated_at",
    ]
    .into_iter()
    .enumerate()
    {
        let selected = page(
            &workspace,
            "needle",
            &["--sort", "title", "--fields", fields],
            &format!("search_fields_projection_{index}"),
        );
        assert_projection(&full, &selected, fields);
        if index == 0 {
            assert!(!selected.to_string().contains("BODY_ONLY_MARKER"));
        }
    }
    assert_eq!(
        full,
        page(
            &workspace,
            "needle",
            &["--sort", "title"],
            "search_fields_full_again"
        )
    );
}

#[test]
fn search_fields_cli_filters_before_page_boundaries() {
    let _log = common::test_log("search_fields_cli_filters_before_page_boundaries");
    let workspace = workspace();
    create(&workspace, "Alpha excluded", "needle skip");
    let beta = create(&workspace, "Beta", "needle KEEP");
    let gamma = create(&workspace, "Gamma", "needle KEEP");
    for id in [&beta, &gamma] {
        let update = run_br(
            &workspace,
            ["update", id, "--notes", "CAFÉ"],
            "search_fields_notes",
        );
        assert!(update.status.success(), "{update:?}");
    }
    for offset in ["0", "1", "2", "99"] {
        let args = [
            "--desc-contains",
            "keep",
            "--notes-contains",
            "café",
            "--sort",
            "title",
            "--limit",
            "1",
            "--offset",
            offset,
        ];
        let full = page(
            &workspace,
            "needle",
            &args,
            &format!("search_fields_filter_full_{offset}"),
        );
        let mut selected_args = args.to_vec();
        selected_args.extend(["--fields", "id,title"]);
        let selected = page(
            &workspace,
            "needle",
            &selected_args,
            &format!("search_fields_filter_{offset}"),
        );
        assert_projection(&full, &selected, "id,title");
        assert_eq!(selected["has_more"], offset == "0");
        if offset == "1" {
            assert_eq!(selected["issues"][0]["id"], gamma);
        }
    }
}

#[test]
fn search_fields_cli_keeps_unicode_comment_matches_and_hidden_history() {
    let _log =
        common::test_log("search_fields_cli_keeps_unicode_comment_matches_and_hidden_history");
    let workspace = workspace();
    create(&workspace, "Alpha", "CAFÉ.[X]%_");
    let handoff = create(&workspace, "Beta", "Unrelated body");
    let comment = run_br(
        &workspace,
        [
            "comments",
            "add",
            &handoff,
            "--message",
            "CAFÉ.[X]%_ historical comment",
        ],
        "search_fields_unicode_comment",
    );
    assert!(comment.status.success(), "{comment:?}");
    let archived = create(&workspace, "Gamma archived", "CAFÉ.[X]%_");
    let closed = run_br(
        &workspace,
        ["close", &archived, "--reason", "Completed"],
        "search_fields_close",
    );
    assert!(closed.status.success(), "{closed:?}");
    for offset in ["0", "1", "2"] {
        let args = ["--sort", "title", "--limit", "1", "--offset", offset];
        let full = page(
            &workspace,
            "café.[x]%_",
            &args,
            &format!("search_fields_unicode_full_{offset}"),
        );
        let mut selected_args = args.to_vec();
        selected_args.extend(["--fields", "id,title,status"]);
        let selected = page(
            &workspace,
            "café.[x]%_",
            &selected_args,
            &format!("search_fields_unicode_{offset}"),
        );
        assert_projection(&full, &selected, "id,title,status");
        assert_eq!(selected["hidden_closed_count"], 1);
    }
    let all = page(
        &workspace,
        "café.[x]%_",
        &["--all", "--limit", "0", "--fields", "id"],
        "search_fields_unicode_all",
    );
    assert_eq!(all["issues"].as_array().unwrap().len(), 3);
    assert_eq!(all["hidden_closed_count"], 0);
}

#[test]
fn search_fields_cli_formats_and_invalid_empty_results() {
    let _log = common::test_log("search_fields_cli_formats_and_invalid_empty_results");
    let workspace = workspace();
    for (index, fields) in ["id,typo", "id,,title", "id,"].into_iter().enumerate() {
        for format in ["json", "toon"] {
            let result = run_br(
                &workspace,
                ["search", "nothing", "--format", format, "--fields", fields],
                &format!("search_fields_invalid_{format}_{index}"),
            );
            assert_eq!(result.status.code(), Some(4), "{result:?}");
            assert!(result.stdout.contains("fields") || result.stderr.contains("fields"));
        }
    }
    let id = create(&workspace, "Needle title", "DO_NOT_EMIT_BODY");
    let toon = run_br(
        &workspace,
        [
            "search", "needle", "--format", "toon", "--fields", "id,title",
        ],
        "search_fields_toon",
    );
    assert!(toon.status.success(), "{toon:?}");
    assert!(toon.stdout.contains(&id), "{toon:?}");
    assert!(toon.stdout.contains("hidden_closed_count: 0"), "{toon:?}");
    assert!(toon.stdout.contains("limit: 50"), "{toon:?}");
    assert!(!toon.stdout.contains("DO_NOT_EMIT_BODY"));
    let plain = run_br(
        &workspace,
        ["search", "needle", "--no-color"],
        "search_fields_plain",
    );
    let ignored = run_br(
        &workspace,
        ["search", "needle", "--no-color", "--fields", "id"],
        "search_fields_plain_ignored",
    );
    assert!(plain.status.success() && ignored.status.success());
    assert_eq!(plain.stdout, ignored.stdout);
    let csv = run_br(
        &workspace,
        [
            "search", "needle", "--format", "csv", "--fields", "id,title",
        ],
        "search_fields_csv",
    );
    assert!(csv.status.success(), "{csv:?}");
    assert!(csv.stdout.contains(&id));
    assert!(!csv.stdout.contains("DO_NOT_EMIT_BODY"));
    let quiet = run_br(
        &workspace,
        ["search", "needle", "--quiet", "--fields", "id"],
        "search_fields_quiet",
    );
    assert!(quiet.status.success(), "{quiet:?}");
    assert!(quiet.stdout.is_empty(), "{quiet:?}");
}

#[test]
fn search_fields_cli_preserves_default_cap_and_unlimited_search() {
    let _log = common::test_log("search_fields_cli_preserves_default_cap_and_unlimited_search");
    let workspace = workspace();
    for number in 0..51 {
        create(
            &workspace,
            &format!("Needle {number:03}"),
            "Body not needed in output",
        );
    }
    let full = page(
        &workspace,
        "needle",
        &["--sort", "title"],
        "search_fields_default_full",
    );
    let selected = page(
        &workspace,
        "needle",
        &["--sort", "title", "--fields", "id,title"],
        "search_fields_default",
    );
    assert_projection(&full, &selected, "id,title");
    assert_eq!(selected["issues"].as_array().unwrap().len(), 50);
    assert_eq!(selected["has_more"], true);
    let all = page(
        &workspace,
        "needle",
        &["--limit", "0", "--fields", "id"],
        "search_fields_unlimited",
    );
    assert_eq!(all["issues"].as_array().unwrap().len(), 51);
    assert_eq!(all["limit"], 0);
    assert_eq!(all["has_more"], false);
}
