//! Real CLI regressions for Unicode search (beads_rust-whnbi).

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use std::collections::BTreeSet;

fn workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let initialized = run_br(&workspace, ["init"], "init_unicode_search");
    assert!(initialized.status.success(), "{initialized:?}");
    workspace
}

fn create(workspace: &BrWorkspace, title: &str, description: &str) -> String {
    let created = run_br(
        workspace,
        [
            "create",
            title,
            "--type",
            "task",
            "--description",
            description,
        ],
        "create_unicode_issue",
    );
    assert!(created.status.success(), "{created:?}");
    let line = created.stdout.lines().next().expect("creation output");
    let normalized = line.strip_prefix("✓ ").unwrap_or(line);
    normalized
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .expect("created issue ID")
        .trim()
        .to_string()
}

fn comment(workspace: &BrWorkspace, id: &str, body: &str) {
    let added = run_br(
        workspace,
        ["comments", "add", id, "--message", body],
        "add_unicode_comment",
    );
    assert!(added.status.success(), "{added:?}");
}

fn search(workspace: &BrWorkspace, query: &str, extra: &[&str], step: &str) -> Value {
    let mut args = vec!["search", query, "--json"];
    args.extend_from_slice(extra);
    let result = run_br(workspace, args, step);
    assert!(result.status.success(), "{result:?}");
    serde_json::from_str(&extract_json_payload(&result.stdout)).expect("search JSON")
}

fn assert_ids(output: &Value, expected: &[&str]) {
    let rows = output["issues"].as_array().expect("search issues array");
    let actual = rows
        .iter()
        .map(|row| row["id"].as_str().expect("issue ID"))
        .collect::<BTreeSet<_>>();
    assert_eq!(rows.len(), expected.len(), "{output}");
    assert_eq!(actual, expected.iter().copied().collect::<BTreeSet<_>>());
}

#[test]
fn unicode_cli_finds_case_variants_and_old_comment_evidence() {
    let _log = common::test_log("unicode_cli_finds_case_variants_and_old_comment_evidence");
    let workspace = workspace();
    let upper = create(&workspace, "Uppercase record", "CAFÉ");
    let lower = create(&workspace, "Lowercase record", "café");
    let handoff = create(&workspace, "Historical handoff", "Other text");
    let control = create(&workspace, "Unrelated record", "cafe without an accent");
    comment(
        &workspace,
        &handoff,
        "CAFÉ is mentioned only in this older handoff",
    );
    comment(
        &workspace,
        &handoff,
        "The latest handoff has no matching word",
    );

    for (index, query) in ["café", "CAFÉ"].into_iter().enumerate() {
        let output = search(
            &workspace,
            query,
            &["--limit", "0"],
            &format!("unicode_case_{index}"),
        );
        assert_ids(&output, &[&upper, &lower, &handoff]);
        assert_eq!(output["hidden_closed_count"], 0);
        assert_eq!(output["has_more"], false);
        let text = run_br(
            &workspace,
            ["search", query, "--limit", "0", "--no-color"],
            &format!("unicode_text_{index}"),
        );
        assert!(text.status.success(), "{text:?}");
        for id in [&upper, &lower, &handoff] {
            assert!(text.stdout.contains(id), "{text:?}");
        }
        assert!(!text.stdout.contains(&control), "{text:?}");
    }
    let filtered = search(
        &workspace,
        "record",
        &["--desc-contains", "CAFÉ", "--limit", "0"],
        "unicode_description_filter",
    );
    assert_ids(&filtered, &[&upper, &lower]);
    let toon = run_br(
        &workspace,
        ["search", "café", "--format", "toon", "--limit", "0"],
        "unicode_toon",
    );
    assert!(toon.status.success(), "{toon:?}");
    for id in [&upper, &lower, &handoff] {
        assert!(toon.stdout.contains(id), "{toon:?}");
    }
    let quiet = run_br(&workspace, ["search", "CAFÉ", "--quiet"], "unicode_quiet");
    assert!(quiet.status.success(), "{quiet:?}");
    assert!(quiet.stdout.is_empty(), "{quiet:?}");
}

#[test]
fn unicode_cli_pages_matching_records_and_discloses_hidden_history() {
    let _log = common::test_log("unicode_cli_pages_matching_records_and_discloses_hidden_history");
    let workspace = workspace();
    let alpha = create(&workspace, "CAFÉ alpha", "keep");
    let beta = create(&workspace, "café beta", "keep");
    let gamma = create(&workspace, "Gamma record", "keep");
    comment(&workspace, &gamma, "CAFÉ comment-only match");
    let closed = create(&workspace, "CAFÉ archived", "Archived evidence");
    let closed_comment = create(&workspace, "Archived handoff", "Other text");
    comment(&workspace, &closed_comment, "CAFÉ archived comment");
    for id in [&closed, &closed_comment] {
        let result = run_br(
            &workspace,
            ["close", id, "--reason", "Completed"],
            "close_unicode",
        );
        assert!(result.status.success(), "{result:?}");
    }

    for (offset, expected, more) in [
        ("1", vec![beta.as_str()], true),
        ("2", vec![gamma.as_str()], false),
        ("3", vec![], false),
    ] {
        let output = search(
            &workspace,
            "CAFÉ",
            &["--sort", "title", "--limit", "1", "--offset", offset],
            &format!("unicode_page_{offset}"),
        );
        assert_ids(&output, &expected);
        assert_eq!(output["has_more"], more);
        assert_eq!(output["limit"], 1);
        assert_eq!(output["hidden_closed_count"], 2);
    }
    let all = search(
        &workspace,
        "café",
        &["--all", "--limit", "0"],
        "unicode_all",
    );
    assert_ids(&all, &[&alpha, &beta, &gamma, &closed, &closed_comment]);
    assert_eq!(all["hidden_closed_count"], 0);
    assert_eq!(all["has_more"], false);
    let csv = run_br(
        &workspace,
        [
            "search", "café", "--format", "csv", "--fields", "id,title", "--sort", "title",
            "--limit", "1", "--offset", "1",
        ],
        "unicode_csv_page",
    );
    assert!(csv.status.success(), "{csv:?}");
    assert!(csv.stdout.contains(&beta), "{csv:?}");
    assert!(!csv.stdout.contains(&alpha), "{csv:?}");
    assert!(!csv.stdout.contains(&closed), "{csv:?}");
    assert!(csv.stderr.contains("more matches exist"), "{csv:?}");
}

#[test]
fn unicode_cli_search_observes_updates_and_treats_punctuation_literally() {
    let _log =
        common::test_log("unicode_cli_search_observes_updates_and_treats_punctuation_literally");
    let workspace = workspace();
    let target = create(&workspace, "Mutable record", "Ordinary text");
    let before = search(&workspace, "café.[x]%_", &[], "unicode_before_update");
    assert_ids(&before, &[]);
    let update = run_br(
        &workspace,
        [
            "update",
            &target,
            "--description",
            "prefix CAFÉ.[X]%_ suffix",
        ],
        "unicode_update",
    );
    assert!(update.status.success(), "{update:?}");
    create(&workspace, "Lookalike record", "CAFÉZxanythingQ");
    let after = search(&workspace, "café.[x]%_", &[], "unicode_after_update");
    assert_ids(&after, &[&target]);
    // ASCII case-insensitive search retains its original behavior.
    let ascii = search(&workspace, "MUTABLE", &[], "ascii_control");
    assert_ids(&ascii, &[&target]);
}
