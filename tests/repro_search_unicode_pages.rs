//! CLI contracts for Unicode match-aware pagination before full hydration.

mod common;

use common::cli::{BrWorkspace, run_br};
use serde_json::Value;

struct Fixture {
    workspace: BrWorkspace,
    high: String,
    middle: String,
    low: String,
    archived: String,
}

fn create(workspace: &BrWorkspace, title: &str, description: &str, priority: &str) -> String {
    let created = run_br(
        workspace,
        [
            "create", title, "--type", "task", "--description", description,
            "--priority", priority,
        ],
        "create_unicode_page_issue",
    );
    assert!(created.status.success(), "{created:?}");
    let line = created.stdout.lines().next().expect("creation output");
    line.strip_prefix("✓ ")
        .unwrap_or(line)
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
        "unicode_page_comment",
    );
    assert!(added.status.success(), "{added:?}");
}

fn fixture() -> Fixture {
    let workspace = BrWorkspace::new();
    let initialized = run_br(&workspace, ["init"], "init_unicode_pages");
    assert!(initialized.status.success(), "{initialized:?}");

    // Distinct priorities make the expected order independent of clock timing.
    // Title order is deliberately different, and one result matches only in an
    // old comment so direct-field hits cannot be emitted ahead of comment hits.
    let low = create(&workspace, "CAFÉ alpha", "keep", "3");
    let high = create(&workspace, "Zulu handoff", "Historical evidence", "0");
    let middle = create(&workspace, "Middle description", "CAFÉ keep", "1");
    create(&workspace, "Nonmatching control", "No accented word", "0");
    let archived = create(&workspace, "Archived handoff", "Archived evidence", "0");
    for body in ["CAFÉ old evidence", "café duplicate evidence", "Newest unrelated note"] {
        comment(&workspace, &high, body);
    }
    comment(&workspace, &archived, "CAFÉ historical evidence");
    comment(&workspace, &archived, "Latest unrelated note");
    let closed = run_br(
        &workspace,
        ["close", &archived, "--reason", "Completed"],
        "close_unicode_page_history",
    );
    assert!(closed.status.success(), "{closed:?}");
    for id in [&high, &low] {
        let updated = run_br(
            &workspace,
            ["update", id, "--notes", "selected FULL_ROW_NOTES"],
            "unicode_page_notes",
        );
        assert!(updated.status.success(), "{updated:?}");
        let labeled = run_br(
            &workspace,
            ["label", "add", id, "selected"],
            "unicode_page_label",
        );
        assert!(labeled.status.success(), "{labeled:?}");
    }
    let dependency = run_br(
        &workspace,
        ["dep", "add", &high, &middle],
        "unicode_page_dependency",
    );
    assert!(dependency.status.success(), "{dependency:?}");
    Fixture { workspace, high, middle, low, archived }
}

fn search(workspace: &BrWorkspace, extra: &[&str], step: &str) -> Value {
    let mut args = vec!["search", "café", "--json"];
    args.extend_from_slice(extra);
    let result = run_br(workspace, args, step);
    assert!(result.status.success(), "{result:?}");
    // Reject stray prose or a partial document; do not extract a JSON fragment.
    serde_json::from_str(&result.stdout).expect("entire search stdout is JSON")
}

fn assert_page(full: &Value, page: &Value, offset: usize, limit: usize) {
    let all_rows = full["issues"].as_array().expect("all matching rows");
    let expected = all_rows
        .iter()
        .skip(offset)
        .take(if limit == 0 { usize::MAX } else { limit })
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(page["issues"], Value::Array(expected), "complete ordered rows");
    assert_eq!(page["limit"], serde_json::json!(limit));
    assert_eq!(page["offset"], serde_json::json!(offset));
    assert_eq!(
        page["has_more"],
        limit > 0 && all_rows.len() > offset.saturating_add(limit)
    );
    assert_eq!(page["hidden_closed_count"], full["hidden_closed_count"]);
}

#[test]
fn unicode_default_pages_preserve_full_rows_and_history_counts() {
    let _log = common::test_log("unicode_default_pages_preserve_full_rows_and_history_counts");
    let f = fixture();
    let full = search(&f.workspace, &["--limit", "0"], "unicode_default_full");
    let rows = full["issues"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["id"], f.high);
    assert_eq!(rows[1]["id"], f.middle);
    assert_eq!(rows[2]["id"], f.low);
    assert_eq!(rows[0]["notes"], "selected FULL_ROW_NOTES");
    assert_eq!(rows[0]["labels"], serde_json::json!(["selected"]));
    assert_eq!(rows[0]["dependency_count"], 1);
    assert_eq!(rows[1]["dependent_count"], 1);
    assert_eq!(full["hidden_closed_count"], 1);
    assert_eq!(full["has_more"], false);

    for (case, (offset, limit)) in [(0, 1), (1, 1), (2, 1), (3, 1), (99, 1), (1, 0), (0, 2)]
        .into_iter()
        .enumerate()
    {
        let offset_arg = offset.to_string();
        let limit_arg = limit.to_string();
        for explicit_sort in [false, true] {
            let mut args = vec!["--offset", offset_arg.as_str(), "--limit", limit_arg.as_str()];
            if explicit_sort {
                args.extend(["--sort", "priority"]);
            }
            let page = search(
                &f.workspace,
                &args,
                &format!("unicode_default_page_{case}_{explicit_sort}"),
            );
            assert_page(&full, &page, offset, limit);
        }
    }
    let all = search(&f.workspace, &["--all", "--limit", "0"], "unicode_all_history");
    assert_eq!(all["issues"].as_array().unwrap().len(), 4);
    assert_eq!(all["hidden_closed_count"], 0);
    assert!(all["issues"].as_array().unwrap().iter().any(|row| row["id"] == f.archived));
}

#[test]
fn unicode_page_fallbacks_filter_and_sort_before_selecting_rows() {
    let _log = common::test_log("unicode_page_fallbacks_filter_and_sort_before_selecting_rows");
    let f = fixture();
    let cases: &[&[&str]] = &[
        &["--notes-contains", "selected"],
        &["--desc-contains", "keep"],
        &["--priority-min", "1"],
        &["--label", "selected"],
        &["--sort", "title"],
        &["--sort", "updated"],
        &["--reverse"],
        &["--desc-contains", "keep", "--reverse"],
    ];
    for (case, filters) in cases.iter().enumerate() {
        let mut args = filters.to_vec();
        args.extend(["--limit", "0"]);
        let full = search(&f.workspace, &args, &format!("unicode_fallback_full_{case}"));
        if case <= 3 {
            assert_eq!(full["issues"].as_array().unwrap().len(), 2);
        }
        for offset in [0, 1, 3] {
            let offset_arg = offset.to_string();
            let mut args = filters.to_vec();
            args.extend(["--limit", "1", "--offset", offset_arg.as_str()]);
            let page = search(
                &f.workspace,
                &args,
                &format!("unicode_fallback_page_{case}_{offset}"),
            );
            assert_page(&full, &page, offset, 1);
        }
    }
}

#[test]
fn unicode_default_page_formats_disclose_truncation_without_extra_rows() {
    let _log = common::test_log("unicode_default_page_formats_disclose_truncation_without_extra_rows");
    let f = fixture();
    for format in ["text", "toon", "csv"] {
        let mut args = vec![
            "search", "café", "--format", format, "--limit", "1", "--offset", "1", "--no-color",
        ];
        if format == "csv" {
            args.extend(["--fields", "id,title"]);
        }
        let result = run_br(&f.workspace, args, &format!("unicode_default_format_{format}"));
        assert!(result.status.success(), "{result:?}");
        assert!(result.stdout.contains(&f.middle), "{result:?}");
        for absent in [&f.high, &f.low, &f.archived] {
            assert!(!result.stdout.contains(absent), "{result:?}");
        }
        match format {
            "toon" => {
                assert!(result.stdout.contains("has_more: true"), "{result:?}");
                assert!(result.stdout.contains("hidden_closed_count: 1"), "{result:?}");
            }
            "csv" => assert!(result.stderr.contains("more matches exist"), "{result:?}"),
            _ => {
                assert!(result.stdout.contains("more matches exist"), "{result:?}");
                assert!(result.stdout.contains("1 closed match(es) hidden"), "{result:?}");
            }
        }
    }
    let quiet = run_br(
        &f.workspace,
        ["search", "café", "--limit", "1", "--quiet"],
        "unicode_default_page_quiet",
    );
    assert!(quiet.status.success(), "{quiet:?}");
    assert!(quiet.stdout.is_empty(), "{quiet:?}");
}
