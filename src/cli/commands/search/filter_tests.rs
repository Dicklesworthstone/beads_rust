//! Mandatory search predicates must not disappear when compilation fails.

use super::*;
use chrono::TimeZone;

fn issue(id: &str, description: Option<&str>, notes: Option<&str>) -> Issue {
    let timestamp = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
    Issue {
        id: id.to_string(),
        title: format!("Issue {id}"),
        description: description.map(str::to_string),
        notes: notes.map(str::to_string),
        created_at: timestamp,
        updated_at: timestamp,
        ..Issue::default()
    }
}

fn ids(issues: &[Issue]) -> Vec<&str> {
    issues.iter().map(|issue| issue.id.as_str()).collect()
}

#[test]
fn text_filter_resource_limits_fail_closed_for_empty_and_nonempty_results() {
    for field in ["desc_contains", "notes_contains"] {
        for rows in [Vec::new(), vec![issue("bd-control", None, None)]] {
            let args = ListArgs {
                desc_contains: (field == "desc_contains").then(|| "needle".to_string()),
                notes_contains: (field == "notes_contains").then(|| "needle".to_string()),
                ..ListArgs::default()
            };
            let mut attempts = 0;
            let error = apply_client_filters_with_compiler(rows, &args, |pattern| {
                attempts += 1;
                // A real compiler resource refusal, not a synthetic predicate
                // result. A zero compiled-size budget needs no huge fixture.
                let result = RegexBuilder::new(pattern)
                    .case_insensitive(true)
                    .size_limit(0)
                    .build();
                assert!(matches!(&result, Err(regex::Error::CompiledTooBig(_))));
                result
            })
            .expect_err("a failed supplied filter must never mean no filter");
            assert_eq!(attempts, 1);
            let BeadsError::Validation {
                field: actual,
                reason,
            } = error
            else {
                panic!("expected a field-specific validation error: {error:?}");
            };
            assert_eq!(actual, field);
            assert!(reason.contains("cannot compile case-insensitive literal filter"));
        }
    }
}

#[test]
fn notes_compilation_failure_is_not_hidden_by_a_valid_description_filter() {
    let args = ListArgs {
        desc_contains: Some("matching".to_string()),
        notes_contains: Some("required".to_string()),
        ..ListArgs::default()
    };
    let mut attempts = 0;
    let result = apply_client_filters_with_compiler(
        vec![issue("bd-match", Some("MATCHING"), None)],
        &args,
        |pattern| {
            attempts += 1;
            let mut builder = RegexBuilder::new(pattern);
            builder.case_insensitive(true);
            if attempts == 2 {
                builder.size_limit(0);
            }
            builder.build()
        },
    );
    assert_eq!(attempts, 2);
    assert!(matches!(
        result,
        Err(BeadsError::Validation { field, .. }) if field == "notes_contains"
    ));
}

#[test]
fn absent_filters_skip_compilation_but_empty_literals_are_still_predicates() {
    let rows = vec![issue("bd-empty", None, None)];
    let unfiltered = apply_client_filters_with_compiler(rows.clone(), &ListArgs::default(), |_| {
        panic!("absent filters must not invoke the compiler")
    })
    .unwrap();
    assert_eq!(ids(&unfiltered), vec!["bd-empty"]);

    let mut patterns = Vec::new();
    let filtered = apply_client_filters_with_compiler(
        rows,
        &ListArgs {
            desc_contains: Some(String::new()),
            notes_contains: Some(String::new()),
            ..ListArgs::default()
        },
        |pattern| {
            patterns.push(pattern.to_string());
            RegexBuilder::new(pattern).case_insensitive(true).build()
        },
    )
    .unwrap();
    assert_eq!(patterns, vec![String::new(), String::new()]);
    assert_eq!(ids(&filtered), vec!["bd-empty"]);
}

#[test]
fn unicode_text_predicates_are_literal_and_all_requested_fields_must_match() {
    let rows = vec![
        issue("bd-match", Some("CAFÉ.[X]%_ evidence"), Some("Σ(1) note")),
        issue("bd-punctuation", Some("CAFÉZxanythingQ"), Some("Σ(1) note")),
        issue("bd-desc-only", Some("CAFÉ.[X]%_ evidence"), Some("other")),
        issue("bd-notes-only", Some("other"), Some("Σ(1) note")),
        issue("bd-absent", None, None),
    ];
    let actual = apply_client_filters(
        rows,
        &ListArgs {
            desc_contains: Some("café.[x]%_".to_string()),
            notes_contains: Some("ς(1)".to_string()),
            ..ListArgs::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&actual), vec!["bd-match"]);
}

#[test]
fn priority_ranges_lists_and_repeated_values_use_the_shared_queue_parser() {
    for values in [
        vec!["0-1"],
        vec!["P0-P1"],
        vec!["0,1"],
        vec!["P1", "0", "1"],
        vec!["0-1", "P1"],
    ] {
        let filters = build_filters(&ListArgs {
            priority: values.into_iter().map(str::to_string).collect(),
            ..ListArgs::default()
        })
        .unwrap();
        assert_eq!(filters.priorities, Some(vec![Priority(0), Priority(1)]));
    }
    assert!(
        build_filters(&ListArgs::default())
            .unwrap()
            .priorities
            .is_none()
    );
}

#[test]
fn malformed_priority_selectors_error_even_when_no_issue_matches() {
    let storage = SqliteStorage::open_memory().unwrap();
    for value in ["4-0", "0-5", "0,bad", "0-1-2"] {
        let args = ListArgs {
            priority: vec![value.to_string()],
            ..ListArgs::default()
        };
        for query in ["needle", "café"] {
            for format in [OutputFormat::Json, OutputFormat::Text] {
                assert!(matches!(
                    collect_search_results_for_output(&storage, query, &args, format),
                    Err(BeadsError::InvalidPriority { .. })
                ));
            }
            assert!(matches!(
                count_hidden_closed_matches(&storage, query, &args),
                Err(BeadsError::InvalidPriority { .. })
            ));
        }
    }
}

#[test]
fn priority_ranges_preserve_comment_matches_pagination_and_hidden_history() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    for priority in 0..=4 {
        for closed in [false, true] {
            let id = format!("bd-{}{priority}", if closed { "c" } else { "o" });
            let body = if priority == 1 {
                "KEEP"
            } else {
                "needle CAFÉ KEEP"
            };
            let mut record = issue(&id, Some(body), Some("CAFÉ notes"));
            record.priority = Priority(priority);
            if closed {
                record.status = Status::Closed;
                record.closed_at = Some(record.updated_at);
            }
            storage.create_issue(&record, "tester").unwrap();
            if priority == 1 {
                storage
                    .add_comment(&id, "tester", "needle CAFÉ older evidence")
                    .unwrap();
                storage
                    .add_comment(&id, "tester", "unrelated newest handoff")
                    .unwrap();
            }
        }
    }

    for query in ["needle", "café"] {
        for client_filter in [false, true] {
            for (limit, offset, shown, more) in [
                (1, 0, 1, true),
                (1, 1, 1, false),
                (1, 99, 0, false),
                (0, 0, 2, false),
                (0, 1, 1, false),
            ] {
                let explicit = ListArgs {
                    priority: vec!["0".to_string(), "1".to_string()],
                    desc_contains: client_filter.then(|| "keep".to_string()),
                    notes_contains: client_filter.then(|| "café".to_string()),
                    limit: Some(limit),
                    offset: Some(offset),
                    ..ListArgs::default()
                };
                let range = ListArgs {
                    priority: vec!["P0-P1".to_string()],
                    ..explicit.clone()
                };
                let expected = collect_search_results_for_output(
                    &storage,
                    query,
                    &explicit,
                    OutputFormat::Json,
                )
                .unwrap();
                let actual =
                    collect_search_results_for_output(&storage, query, &range, OutputFormat::Json)
                        .unwrap();
                assert_eq!(
                    serde_json::to_value(&actual.issues).unwrap(),
                    serde_json::to_value(&expected.issues).unwrap()
                );
                assert_eq!(actual.issues.len(), shown);
                assert_eq!(
                    (actual.limit, actual.offset, actual.has_more),
                    (limit, offset, more)
                );
                assert_eq!(
                    count_hidden_closed_matches(&storage, query, &range).unwrap(),
                    2
                );
                if offset == 1 {
                    assert_eq!(ids(&actual.issues), vec!["bd-o1"]);
                }
            }
        }
        let args = ListArgs {
            priority: vec!["0-1".to_string()],
            all: true,
            limit: Some(0),
            ..ListArgs::default()
        };
        let page =
            collect_search_results_for_output(&storage, query, &args, OutputFormat::Json).unwrap();
        assert_eq!(ids(&page.issues), vec!["bd-c0", "bd-o0", "bd-c1", "bd-o1"]);
        assert_eq!(
            count_hidden_closed_matches(&storage, query, &args).unwrap(),
            0
        );
    }
}
