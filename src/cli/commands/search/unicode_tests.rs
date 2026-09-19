//! Unicode search regressions for beads_rust-whnbi.
//!
//! Exercise the actual command collector with storage, not a replacement SQL
//! model. The low-level storage search API remains ASCII-folded; br search
//! deliberately routes non-ASCII queries through its client-side matcher.

use super::*;
use chrono::TimeZone;

fn issue(id: &str, title: &str, description: Option<&str>) -> Issue {
    let timestamp = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    Issue {
        id: id.to_string(),
        title: title.to_string(),
        description: description.map(str::to_string),
        created_at: timestamp,
        updated_at: timestamp,
        ..Issue::default()
    }
}

fn ids(issues: &[Issue]) -> Vec<&str> {
    issues.iter().map(|issue| issue.id.as_str()).collect()
}

fn unlimited() -> ListArgs {
    ListArgs {
        limit: Some(0),
        ..ListArgs::default()
    }
}

#[test]
fn unicode_case_variants_match_fields_and_all_comment_history_in_every_format() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    for item in [
        issue("bd-a", "CAFÉ title", None),
        issue("bd-b", "café title", None),
        issue("bd-c", "Description match", Some("Visit the CAFÉ")),
        issue("bd-d", "Comment match", None),
        issue("bd-e", "Unrelated", None),
    ] {
        storage.create_issue(&item, "tester").unwrap();
    }
    storage.add_comment("bd-d", "tester", "CAFÉ handoff").unwrap();
    storage
        .add_comment("bd-d", "tester", "café duplicate hit")
        .unwrap();
    storage
        .add_comment("bd-d", "tester", "Newest comment has no match")
        .unwrap();
    storage
        .add_comment("bd-a", "tester", "CAFÉ duplicate field hit")
        .unwrap();

    for query in ["café", "CAFÉ", "CaFé", " CAFÉ "] {
        for format in [
            OutputFormat::Text,
            OutputFormat::Json,
            OutputFormat::Toon,
            OutputFormat::Csv,
        ] {
            let page =
                collect_search_results_for_output(&storage, query, &unlimited(), format).unwrap();
            assert_eq!(
                ids(&page.issues),
                vec!["bd-a", "bd-b", "bd-c", "bd-d"],
                "{query}"
            );
            assert!(!page.has_more);
            assert!(page.issues.iter().all(|issue| issue.comments.is_empty()));
        }
    }
    assert!(
        collect_search_results(&storage, "absent-é", &unlimited())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn unicode_field_matcher_covers_ids_and_non_latin_case_without_searching_notes() {
    for (query, text) in [("café", "CAFÉ"), ("σ", "ς"), ("я", "Я")] {
        let matcher = RegexBuilder::new(&regex::escape(query))
            .case_insensitive(true)
            .build()
            .unwrap();
        for item in [
            issue(&format!("bd-{text}"), "Other", None),
            issue("bd-title", text, None),
            issue("bd-description", "Other", Some(text)),
        ] {
            assert!(
                unicode_issue_fields_match(&item, &matcher),
                "{query}/{text}"
            );
        }
        let mut notes_only = issue("bd-notes", "Other", None);
        notes_only.notes = Some(text.to_string());
        assert!(!unicode_issue_fields_match(&notes_only, &matcher));
    }
}

#[test]
fn unicode_queries_are_literal_substrings_not_regular_expressions_or_like_patterns() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    for item in [
        issue("bd-literal", "prefix CAFÉ.[X]%_ suffix", None),
        issue("bd-lookalike", "prefix CAFÉZxanythingQ suffix", None),
    ] {
        storage.create_issue(&item, "tester").unwrap();
    }
    let results = collect_search_results(&storage, "café.[x]%_", &unlimited()).unwrap();
    assert_eq!(ids(&results), vec!["bd-literal"]);
}

#[test]
fn unicode_matching_precedes_sort_offset_limit_and_truncation_probe() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let mut nonmatch = issue("bd-first", "First candidate is not a match", None);
    nonmatch.priority = Priority::CRITICAL;
    for item in [
        nonmatch,
        issue("bd-a", "CAFÉ alpha", Some("keep")),
        issue("bd-b", "café beta", Some("keep")),
        issue("bd-c", "gamma", Some("keep")),
        issue(
            "bd-filtered",
            "CAFÉ excluded by description filter",
            Some("discard"),
        ),
    ] {
        storage.create_issue(&item, "tester").unwrap();
    }
    storage
        .add_comment("bd-c", "tester", "CAFÉ comment-only match")
        .unwrap();
    for reverse in [false, true] {
        let expected = if reverse {
            ["bd-c", "bd-b", "bd-a"]
        } else {
            ["bd-a", "bd-b", "bd-c"]
        };
        for offset in [0, 1, 2, 3, 99] {
            let args = ListArgs {
                sort: Some("title".to_string()),
                reverse,
                desc_contains: Some("keep".to_string()),
                offset: Some(offset),
                limit: Some(1),
                ..ListArgs::default()
            };
            for format in [OutputFormat::Text, OutputFormat::Json] {
                let page =
                    collect_search_results_for_output(&storage, "CAFÉ", &args, format).unwrap();
                let expected_ids = expected.get(offset).copied().into_iter().collect::<Vec<_>>();
                assert_eq!(
                    ids(&page.issues),
                    expected_ids,
                    "offset={offset}, reverse={reverse}"
                );
                assert_eq!(page.has_more, offset < 2);
                assert_eq!(page.limit, 1);
                assert_eq!(page.offset, offset);
            }
        }
    }
    // Without extra client filters, Unicode matching alone still postpones pagination.
    let args = ListArgs {
        limit: Some(1),
        ..ListArgs::default()
    };
    let page =
        collect_search_results_for_output(&storage, "café", &args, OutputFormat::Json).unwrap();
    assert_eq!(ids(&page.issues), vec!["bd-a"]);
    assert!(page.has_more);
}

#[test]
fn unicode_search_crosses_comment_batches_and_preserves_default_page_size() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    for index in 0..258 {
        let id = format!("bd-{index:04}");
        let title = if index < DEFAULT_SEARCH_LIMIT + 1 {
            "CAFÉ"
        } else {
            "Other"
        };
        storage
            .create_issue(&issue(&id, title, None), "tester")
            .unwrap();
    }
    storage
        .add_comment("bd-0257", "tester", "CAFÉ beyond the first comment batch")
        .unwrap();
    let page = collect_search_results_for_output(
        &storage,
        "café",
        &ListArgs::default(),
        OutputFormat::Text,
    )
    .unwrap();
    assert_eq!(page.issues.len(), DEFAULT_SEARCH_LIMIT);
    assert!(page.has_more);
    let all =
        collect_search_results_for_output(&storage, "CAFÉ", &unlimited(), OutputFormat::Json)
            .unwrap();
    assert_eq!(all.issues.len(), DEFAULT_SEARCH_LIMIT + 2);
    assert_eq!(all.issues.last().unwrap().id, "bd-0257");
    assert!(!all.has_more);
}

#[test]
fn unicode_hidden_closed_counts_include_comment_hits_and_respect_filters() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    for (id, title, description, status, template) in [
        ("bd-open", "CAFÉ", Some("CAFÉ"), Status::Open, false),
        ("bd-field", "CAFÉ", Some("CAFÉ"), Status::Closed, false),
        ("bd-comment", "History", None, Status::Closed, false),
        ("bd-other", "café", None, Status::Closed, false),
        ("bd-template", "CAFÉ", None, Status::Closed, true),
    ] {
        let mut item = issue(id, title, description);
        item.status = status;
        item.is_template = template;
        if item.status == Status::Closed {
            item.closed_at = Some(item.updated_at);
        }
        storage.create_issue(&item, "tester").unwrap();
    }
    storage
        .add_comment("bd-comment", "tester", "CAFÉ archived evidence")
        .unwrap();
    storage
        .add_comment("bd-comment", "tester", "café another hit, not another issue")
        .unwrap();
    for id in ["bd-field", "bd-comment"] {
        storage.add_label(id, "chosen", "tester").unwrap();
    }
    for query in ["café", "CAFÉ"] {
        assert_eq!(
            count_hidden_closed_matches(&storage, query, &ListArgs::default()).unwrap(),
            3
        );
        let mut args = ListArgs {
            label: vec!["chosen".to_string()],
            limit: Some(1),
            offset: Some(99),
            ..ListArgs::default()
        };
        assert_eq!(count_hidden_closed_matches(&storage, query, &args).unwrap(), 2);
        args.desc_contains = Some("café".to_string());
        assert_eq!(count_hidden_closed_matches(&storage, query, &args).unwrap(), 1);
        args.all = true;
        assert_eq!(count_hidden_closed_matches(&storage, query, &args).unwrap(), 0);
        args.all = false;
        args.overdue = true;
        assert_eq!(count_hidden_closed_matches(&storage, query, &args).unwrap(), 0);
        let all = collect_search_results(
            &storage,
            query,
            &ListArgs {
                all: true,
                ..unlimited()
            },
        )
        .unwrap();
        assert_eq!(
            ids(&all),
            vec!["bd-comment", "bd-field", "bd-open", "bd-other"]
        );
    }
}

#[test]
fn unicode_main_query_agrees_with_description_filter_and_composes_with_sql_filters() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    for item in [
        issue("bd-a", "alpha one", Some("CAFÉ")),
        issue("bd-b", "alpha two", Some("café")),
        issue("bd-control", "alpha control", Some("ordinary")),
    ] {
        storage.create_issue(&item, "tester").unwrap();
    }
    storage.add_label("bd-a", "chosen", "tester").unwrap();
    let main = collect_search_results(&storage, "café", &unlimited()).unwrap();
    let desc_args = ListArgs {
        desc_contains: Some("CAFÉ".to_string()),
        ..unlimited()
    };
    let desc = collect_search_results(&storage, "alpha", &desc_args).unwrap();
    assert_eq!(ids(&main), ids(&desc));
    assert_eq!(ids(&main), vec!["bd-a", "bd-b"]);
    let args = ListArgs {
        status: vec!["open".to_string()],
        type_: vec!["task".to_string()],
        label: vec!["chosen".to_string()],
        desc_contains: Some("CAFÉ".to_string()),
        priority_min: Some(2),
        priority_max: Some(2),
        limit: Some(1),
        ..ListArgs::default()
    };
    let selected = collect_search_results(&storage, "CAFÉ", &args).unwrap();
    assert_eq!(ids(&selected), vec!["bd-a"]);
}
