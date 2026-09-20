//! Search priority selectors must preserve the requested corpus and its pages.

mod common;

use common::cli::{BrWorkspace, run_br};
use serde_json::Value;

fn workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let result = run_br(&workspace, ["init"], "search_filters_init");
    assert!(result.status.success(), "{result:?}");
    workspace
}

fn create(workspace: &BrWorkspace, title: &str, description: &str, priority: &str) -> String {
    let result = run_br(
        workspace,
        [
            "create",
            title,
            "--type",
            "task",
            "--description",
            description,
            "--priority",
            priority,
        ],
        "search_filters_create",
    );
    assert!(result.status.success(), "{result:?}");
    let line = result.stdout.lines().next().expect("creation output");
    let id = line
        .strip_prefix("✓ ")
        .unwrap_or(line)
        .strip_prefix("Created ")
        .and_then(|text| text.split(':').next())
        .expect("created issue ID")
        .trim()
        .to_string();
    let update = run_br(
        workspace,
        ["update", &id, "--notes", "CAFÉ handoff"],
        "search_filters_notes",
    );
    assert!(update.status.success(), "{update:?}");
    id
}

fn populate(workspace: &BrWorkspace) -> [String; 3] {
    let first = create(workspace, "Alpha", "needle CAFÉ KEEP", "0");
    let second = create(workspace, "Beta", "KEEP", "1");
    let excluded = create(workspace, "Gamma", "needle CAFÉ KEEP", "2");
    for message in ["needle CAFÉ older evidence", "unrelated newest handoff"] {
        let result = run_br(
            workspace,
            ["comments", "add", &second, "--message", message],
            "search_filters_comment",
        );
        assert!(result.status.success(), "{result:?}");
    }
    for (title, priority) in [("Archived selected", "1"), ("Archived excluded", "2")] {
        let id = create(workspace, title, "needle CAFÉ KEEP", priority);
        let result = run_br(
            workspace,
            ["close", &id, "--reason", "Completed"],
            "search_filters_close",
        );
        assert!(result.status.success(), "{result:?}");
    }
    [first, second, excluded]
}

fn page(workspace: &BrWorkspace, query: &str, extra: &[&str], step: &str) -> Value {
    let mut args = vec!["search", query, "--json"];
    args.extend_from_slice(extra);
    let result = run_br(workspace, args, step);
    assert!(result.status.success(), "{result:?}");
    serde_json::from_str(&result.stdout).expect("the whole stdout is one JSON document")
}

#[test]
fn search_priority_ranges_match_explicit_sets_with_comment_only_and_closed_matches() {
    let _log = common::test_log("search_priority_ranges_match_explicit_sets");
    let workspace = workspace();
    let [first, second, _] = populate(&workspace);
    for (query_index, query) in ["needle", "café"].into_iter().enumerate() {
        let expected = page(
            &workspace,
            query,
            &[
                "--priority",
                "0",
                "--priority",
                "1",
                "--sort",
                "title",
                "--limit",
                "0",
            ],
            &format!("search_priority_explicit_{query_index}"),
        );
        assert_eq!(expected["issues"].as_array().unwrap().len(), 2);
        assert_eq!(expected["issues"][0]["id"], first);
        assert_eq!(expected["issues"][1]["id"], second);
        assert_eq!(expected["hidden_closed_count"], 1);
        for (index, mut args) in [
            vec!["--priority", "0-1"],
            vec!["--priority", "P0-P1"],
            vec!["--priority", "0,1"],
            vec!["--priority", "P1", "--priority", "0", "--priority", "1"],
        ]
        .into_iter()
        .enumerate()
        {
            args.extend(["--sort", "title", "--limit", "0"]);
            let actual = page(
                &workspace,
                query,
                &args,
                &format!("search_priority_range_{query_index}_{index}"),
            );
            assert_eq!(actual, expected);
        }
        let all = page(
            &workspace,
            query,
            &["--priority", "0-1", "--all", "--limit", "0"],
            &format!("search_priority_all_{query_index}"),
        );
        assert_eq!(all["issues"].as_array().unwrap().len(), 3);
        assert_eq!(all["hidden_closed_count"], 0);
    }
}

#[test]
fn search_priority_ranges_compose_with_text_filters_before_pagination() {
    let _log =
        common::test_log("search_priority_ranges_compose_with_text_filters_before_pagination");
    let workspace = workspace();
    populate(&workspace);
    for (query_index, query) in ["needle", "café"].into_iter().enumerate() {
        let common = [
            "--priority",
            "P0-P1",
            "--desc-contains",
            "keep",
            "--notes-contains",
            "café",
            "--sort",
            "title",
            "--fields",
            "id,title",
        ];
        let mut unlimited_args = common.to_vec();
        unlimited_args.extend(["--limit", "0"]);
        let unlimited = page(
            &workspace,
            query,
            &unlimited_args,
            &format!("search_priority_unlimited_{query_index}"),
        );
        assert_eq!(unlimited["issues"].as_array().unwrap().len(), 2);
        for (offset, text) in [(0_usize, "0"), (1, "1"), (2, "2"), (99, "99")] {
            let mut args = common.to_vec();
            args.extend(["--limit", "1", "--offset", text]);
            let actual = page(
                &workspace,
                query,
                &args,
                &format!("search_priority_page_{query_index}_{offset}"),
            );
            let mut expected = unlimited.clone();
            expected["issues"] = Value::Array(
                unlimited["issues"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .skip(offset)
                    .take(1)
                    .cloned()
                    .collect(),
            );
            expected["limit"] = Value::from(1);
            expected["offset"] = Value::from(offset);
            expected["has_more"] = Value::from(offset == 0);
            assert_eq!(actual, expected);
            assert_eq!(actual["hidden_closed_count"], 1);
        }
    }
}

#[test]
fn search_rejects_malformed_priorities_in_every_format_even_on_an_empty_workspace() {
    let _log = common::test_log("search_rejects_malformed_priorities_in_every_format");
    let workspace = workspace();
    for (index, priority) in ["4-0", "0-5", "0,bad", "0-1-2"].into_iter().enumerate() {
        for format in ["text", "json", "toon", "csv"] {
            let result = run_br(
                &workspace,
                [
                    "search",
                    "needle",
                    "--format",
                    format,
                    "--priority",
                    priority,
                ],
                &format!("search_priority_invalid_{format}_{index}"),
            );
            assert_eq!(result.status.code(), Some(4), "{result:?}");
            let diagnostic = format!("{} {}", result.stdout, result.stderr).to_lowercase();
            assert!(diagnostic.contains("priority"), "{result:?}");
            assert!(
                !result.stdout.contains("\"issues\""),
                "no successful page: {result:?}"
            );
        }
    }
}

#[test]
fn search_priority_ranges_work_in_text_toon_csv_and_quiet_output() {
    let _log = common::test_log("search_priority_ranges_work_in_text_toon_csv_and_quiet_output");
    let workspace = workspace();
    let [first, second, excluded] = populate(&workspace);
    for format in ["text", "toon", "csv"] {
        let result = run_br(
            &workspace,
            [
                "search",
                "needle",
                "--priority",
                "0-1",
                "--format",
                format,
                "--fields",
                "id,title",
                "--no-color",
            ],
            &format!("search_priority_format_{format}"),
        );
        assert!(result.status.success(), "{result:?}");
        assert!(result.stdout.contains(&first), "{result:?}");
        assert!(result.stdout.contains(&second), "{result:?}");
        assert!(!result.stdout.contains(&excluded), "{result:?}");
    }
    let quiet = run_br(
        &workspace,
        ["search", "needle", "--priority", "P0-P1", "--quiet"],
        "search_priority_quiet",
    );
    assert!(quiet.status.success(), "{quiet:?}");
    assert!(quiet.stdout.is_empty(), "{quiet:?}");
}
